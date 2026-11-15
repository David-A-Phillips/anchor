//! Thread-safe incremental parsing with a bounded cache of syntax trees.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;

use lru::LruCache;
use parking_lot::Mutex;
use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::base::{ContentHash, Error, Result};

use super::Language;
use super::config::ParserConfig;
use super::extract::extract;
use super::model::ParsedFile;

/// Symbols that appeared, disappeared or changed between two parses of the same file, by
/// qualified name. For the first parse of a key every symbol is `added`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymbolDelta {
    /// Qualified names present now and not before (source order).
    pub added: Vec<String>,
    /// Qualified names present before and not now (previous source order).
    pub removed: Vec<String>,
    /// Qualified names present in both whose content hash changed (source order).
    pub modified: Vec<String>,
}

impl SymbolDelta {
    /// `true` when no symbol was added, removed or modified.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    fn between(previous: &[(String, ContentHash)], current: &ParsedFile) -> Self {
        let before: HashMap<&str, &ContentHash> =
            previous.iter().map(|(n, h)| (n.as_str(), h)).collect();
        let after: HashMap<&str, &ContentHash> = current
            .symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), &s.content_hash))
            .collect();
        let mut delta = SymbolDelta::default();
        for symbol in &current.symbols {
            match before.get(symbol.qualified_name.as_str()) {
                None => delta.added.push(symbol.qualified_name.clone()),
                Some(hash) if **hash != symbol.content_hash => {
                    delta.modified.push(symbol.qualified_name.clone());
                }
                Some(_) => {}
            }
        }
        for (name, _) in previous {
            if !after.contains_key(name.as_str()) {
                delta.removed.push(name.clone());
            }
        }
        delta
    }
}

/// Result of [`IncrementalParser::parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseOutcome {
    /// Everything extracted from the new text.
    pub parsed: ParsedFile,
    /// `true` when a cached tree of the same language was used for an incremental re-parse (also
    /// when the text did not change at all).
    pub reused_tree: bool,
    /// Byte ranges of the new text whose syntax structure changed. Without a cached tree this is
    /// one range covering the whole text (empty for empty text).
    pub changed_ranges: Vec<Range<usize>>,
    /// Symbol changes relative to the previous parse of the same key.
    pub delta: SymbolDelta,
    /// `true` when the text exceeded [`ParserConfig::max_file_bytes`] and was not parsed.
    pub skipped_too_large: bool,
}

struct CacheEntry {
    language: Language,
    text: String,
    tree: Tree,
    parsed: ParsedFile,
}

/// Parses source files and keeps their syntax trees so that small edits re-parse in
/// proportion to the edit. Safe to share between threads; parsing is CPU-bound, so call it from
/// a blocking thread.
pub struct IncrementalParser {
    max_file_bytes: u64,
    cache: Mutex<LruCache<String, CacheEntry>>,
    pools: [Mutex<Vec<Parser>>; Language::ALL.len()],
}

impl IncrementalParser {
    /// Creates a parser with the limits in `config`.
    pub fn new(config: &ParserConfig) -> Self {
        let capacity =
            NonZeroUsize::new(config.tree_cache_entries.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            max_file_bytes: config.max_file_bytes,
            cache: Mutex::new(LruCache::new(capacity)),
            pools: std::array::from_fn(|_| Mutex::new(Vec::new())),
        }
    }

    /// Parses `text` as `language`, re-using the cached tree of `key` when there is one.
    pub fn parse(&self, key: &str, language: Language, text: &str) -> Result<ParseOutcome> {
        if text.len() as u64 > self.max_file_bytes {
            self.forget(key);
            return Ok(ParseOutcome {
                parsed: ParsedFile::empty(language),
                reused_tree: false,
                changed_ranges: Vec::new(),
                delta: SymbolDelta::default(),
                skipped_too_large: true,
            });
        }
        // Taken out of the cache while in use: concurrent parses of one key cannot corrupt a
        // tree, the last one to finish wins the slot.
        let cached = self
            .cache
            .lock()
            .pop(key)
            .filter(|entry| entry.language == language);
        let previous: Vec<(String, ContentHash)> = cached
            .as_ref()
            .map(|entry| {
                entry
                    .parsed
                    .symbols
                    .iter()
                    .map(|s| (s.qualified_name.clone(), s.content_hash))
                    .collect()
            })
            .unwrap_or_default();
        let had_cache = cached.is_some();

        if let Some(entry) = cached.as_ref()
            && entry.text == text
        {
            let parsed = entry.parsed.clone();
            if let Some(entry) = cached {
                self.cache.lock().put(key.to_owned(), entry);
            }
            return Ok(ParseOutcome {
                parsed,
                reused_tree: true,
                changed_ranges: Vec::new(),
                delta: SymbolDelta::default(),
                skipped_too_large: false,
            });
        }

        let (tree, changed_ranges) = self.with_parser(language, |parser| match cached {
            Some(mut entry) => {
                if let Some(edit) = compute_edit(&entry.text, text) {
                    entry.tree.edit(&edit);
                }
                // Tree-sitter's error recovery is not guaranteed to produce the same tree
                // incrementally as from scratch. When either side of the edit has syntax errors,
                // re-parse from scratch so results never depend on the edit history.
                let reuse = !entry.tree.root_node().has_error();
                let mut tree = parser.parse(text, reuse.then_some(&entry.tree));
                if tree
                    .as_ref()
                    .is_some_and(|new| reuse && new.root_node().has_error())
                {
                    tree = parser.parse(text, None);
                }
                let ranges = tree
                    .as_ref()
                    .map(|new| {
                        entry
                            .tree
                            .changed_ranges(new)
                            .map(|r| r.start_byte..r.end_byte)
                            .collect()
                    })
                    .unwrap_or_default();
                (tree, ranges)
            }
            None => {
                let ranges = if text.is_empty() {
                    Vec::new()
                } else {
                    std::iter::once(0..text.len()).collect()
                };
                (parser.parse(text, None), ranges)
            }
        })?;
        let tree = tree.ok_or_else(|| Error::parse(format!("{language} parser gave no tree")))?;
        let parsed = extract(language, &tree, text);
        let delta = SymbolDelta::between(&previous, &parsed);
        self.cache.lock().put(
            key.to_owned(),
            CacheEntry {
                language,
                text: text.to_owned(),
                tree,
                parsed: parsed.clone(),
            },
        );
        Ok(ParseOutcome {
            parsed,
            reused_tree: had_cache,
            changed_ranges,
            delta,
            skipped_too_large: false,
        })
    }

    /// Drops the cached tree of `key` (file deleted, or about to be rewritten wholesale).
    pub fn forget(&self, key: &str) {
        self.cache.lock().pop(key);
    }

    /// Number of cached trees.
    pub fn cached_files(&self) -> usize {
        self.cache.lock().len()
    }

    fn with_parser<R>(&self, language: Language, work: impl FnOnce(&mut Parser) -> R) -> Result<R> {
        let pool = &self.pools[language.index()];
        let pooled = pool.lock().pop();
        let mut parser = match pooled {
            Some(parser) => parser,
            None => new_parser(language)?,
        };
        let result = work(&mut parser);
        parser.reset();
        pool.lock().push(parser);
        Ok(result)
    }
}

fn ts_language(language: Language) -> tree_sitter::Language {
    match language {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
    }
}

fn new_parser(language: Language) -> Result<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&ts_language(language))
        .map_err(|e| Error::parse(format!("cannot load the {language} grammar: {e}")))?;
    Ok(parser)
}

/// Parses `text` from scratch, without any cache.
pub fn parse_source(language: Language, text: &str) -> Result<ParsedFile> {
    let mut parser = new_parser(language)?;
    let tree = parser
        .parse(text, None)
        .ok_or_else(|| Error::parse(format!("{language} parser gave no tree")))?;
    Ok(extract(language, &tree, text))
}

fn advance(mut point: Point, segment: &str) -> Point {
    let newlines = segment.bytes().filter(|b| *b == b'\n').count();
    if newlines == 0 {
        point.column += segment.len();
    } else {
        point.row += newlines;
        let tail = segment.rfind('\n').map_or(0, |i| i + 1);
        point.column = segment.len() - tail;
    }
    point
}

/// The smallest single edit turning `old` into `new` (common prefix and suffix), or `None` when
/// they are identical. Offsets never fall inside a UTF-8 sequence; points count rows by `\n` and
/// columns in bytes, as Tree-sitter does (so `\r\n` needs no special case).
pub(crate) fn compute_edit(old: &str, new: &str) -> Option<InputEdit> {
    if old == new {
        return None;
    }
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let mut prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let limit = a.len().min(b.len()) - prefix;
    let mut suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(limit)
        .take_while(|(x, y)| x == y)
        .count();
    while suffix > 0
        && !(old.is_char_boundary(a.len() - suffix) && new.is_char_boundary(b.len() - suffix))
    {
        suffix -= 1;
    }
    let old_end = a.len() - suffix;
    let new_end = b.len() - suffix;
    let start_position = advance(Point::new(0, 0), &old[..prefix]);
    Some(InputEdit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position,
        old_end_position: advance(start_position, &old[prefix..old_end]),
        new_end_position: advance(start_position, &new[prefix..new_end]),
    })
}
