//! Conversion of a parsed file into an ingestion [`Unit`].

use std::collections::BTreeSet;

use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Lookup, NodeAssertion, NodeKey, NodeKind, SourceId, Unit,
    UnitKind, attrs, keys,
};

use super::model::{Reference, ReferenceKind, Symbol};
use super::{Language, ParsedFile};

/// Code chunks are cut at a line boundary once they exceed either bound.
const MAX_CODE_CHUNK_CHARS: usize = 1200;
const MAX_CODE_CHUNK_LINES: usize = 40;
/// Confidence of an import of an external package: the short name may match several services.
const EXTERNAL_IMPORT_WEIGHT: f32 = 0.5;

/// Builds the ingestion unit of one source file: its File node, a node per symbol, containment,
/// call, reference and import edges, and documentation and code chunks.
///
/// The repository's Service node (`keys::service(source)`) is referenced by key only; the
/// ingestion pipeline asserts it. The result is canonical and deterministic.
pub fn build_file_unit(source: &SourceId, rel_path: &str, text: &str, parsed: &ParsedFile) -> Unit {
    let path = keys::normalize_path(rel_path);
    let mut unit = Unit::new(
        keys::unit(source, UnitKind::File, &path),
        source.clone(),
        UnitKind::File,
    );
    let language = parsed.language;

    let file_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let file_key = keys::file(source, &path);
    let mut file_node = NodeAssertion::new(NodeKind::File, file_key.clone(), file_name)
        .alias(path.clone())
        .attr(attrs::PATH, path.clone())
        .attr(attrs::LANGUAGE, language.name());
    let without_extension = strip_extension(&path);
    if without_extension != path {
        file_node = file_node.alias(without_extension);
    }
    unit.add_node(file_node);
    unit.add_edge(EdgeAssertion::new(
        keys::service(source.as_str()),
        EdgeKind::Contains,
        file_key.clone(),
    ));

    let symbol_keys: Vec<NodeKey> = parsed
        .symbols
        .iter()
        .map(|s| keys::symbol(source, &path, &s.qualified_name))
        .collect();
    for (symbol, key) in parsed.symbols.iter().zip(&symbol_keys) {
        unit.add_node(symbol_node(symbol, key, &path, language));
        let parent = symbol.parent.and_then(|p| symbol_keys.get(p));
        unit.add_edge(EdgeAssertion::new(
            parent.unwrap_or(&file_key).clone(),
            EdgeKind::Contains,
            key.clone(),
        ));
        if let Some(doc) = &symbol.doc {
            unit.add_chunk(key, ChunkKind::Docstring, doc.clone());
        }
        unit.add_chunk(key, ChunkKind::Code, code_preview(symbol, text));
    }
    if let Some(doc) = &parsed.module_doc {
        unit.add_chunk(&file_key, ChunkKind::Docstring, doc.clone());
    }

    let mut seen: BTreeSet<(NodeKey, EdgeKind, String)> = BTreeSet::new();
    let mut link =
        |unit: &mut Unit, from: &NodeKey, kind: EdgeKind, lookup: Lookup, weight: f32| {
            if seen.insert((
                from.clone(),
                kind,
                format!("{:?}:{}", lookup.kinds, lookup.text),
            )) {
                unit.add_edge(EdgeAssertion::new(from.clone(), kind, lookup).weight(weight));
            }
        };
    let scoped = |kinds: &[NodeKind], text: String| {
        Lookup::new(kinds, text)
            .in_source(source.clone())
            .near_path(path.clone())
    };

    for reference in &parsed.references {
        let from = reference
            .from_symbol
            .and_then(|i| symbol_keys.get(i))
            .unwrap_or(&file_key);
        let kind = match reference.kind {
            ReferenceKind::Call | ReferenceKind::Macro if reference.from_symbol.is_some() => {
                EdgeKind::Calls
            }
            _ => EdgeKind::References,
        };
        let lookup = scoped(&[NodeKind::Symbol], target_text(reference));
        link(&mut unit, from, kind, lookup, 1.0);
    }

    for import in &parsed.imports {
        for name in &import.names {
            let lookup = scoped(&[NodeKind::Symbol], name.clone());
            link(&mut unit, &file_key, EdgeKind::References, lookup, 1.0);
        }
        if import.relative {
            for target in relative_targets(language, &path, &import.path, &import.names) {
                let lookup = scoped(&[NodeKind::File], target);
                link(&mut unit, &file_key, EdgeKind::Imports, lookup, 1.0);
            }
        } else if let Some(package) = package_short_name(language, &import.path) {
            let lookup = Lookup::new(&[NodeKind::Service], package);
            link(
                &mut unit,
                &file_key,
                EdgeKind::Imports,
                lookup,
                EXTERNAL_IMPORT_WEIGHT,
            );
        }
    }
    unit.finish()
}

fn symbol_node(symbol: &Symbol, key: &NodeKey, path: &str, language: Language) -> NodeAssertion {
    NodeAssertion::new(NodeKind::Symbol, key.clone(), symbol.name.clone())
        .alias(symbol.name.clone())
        .alias(symbol.qualified_name.clone())
        .attr(attrs::PATH, path)
        .attr(attrs::LINE_START, u64::from(symbol.line_start))
        .attr(attrs::LINE_END, u64::from(symbol.line_end))
        .attr(attrs::SYMBOL_KIND, symbol.kind.as_str())
        .attr(attrs::SIGNATURE, symbol.signature.clone())
        .attr(attrs::VISIBILITY, symbol.visibility.as_str())
        .attr(attrs::LANGUAGE, language.name())
}

/// A bounded preview of the symbol's source text, which starts with its signature.
fn code_preview(symbol: &Symbol, text: &str) -> String {
    let Some(source) = text.get(symbol.byte_range.clone()) else {
        return symbol.signature.clone();
    };
    let mut preview = String::new();
    let mut chars = 0;
    for (count, line) in source.lines().enumerate() {
        let extra = line.chars().count() + 1;
        if count >= MAX_CODE_CHUNK_LINES || (count > 0 && chars + extra > MAX_CODE_CHUNK_CHARS) {
            break;
        }
        if count > 0 {
            preview.push('\n');
        }
        preview.push_str(line);
        chars += extra;
    }
    if preview.chars().count() > MAX_CODE_CHUNK_CHARS {
        preview = preview.chars().take(MAX_CODE_CHUNK_CHARS).collect();
    }
    preview
}

/// `Ledger::charge` for a path qualifier, the bare name otherwise.
fn target_text(reference: &Reference) -> String {
    if reference.qualifier_is_path
        && let Some(qualifier) = &reference.qualifier
    {
        let last = qualifier.rsplit(['.', ':']).next().unwrap_or("");
        if !last.is_empty() && !matches!(last, "Self" | "self" | "super" | "crate") {
            return format!("{last}::{}", reference.name);
        }
    }
    reference.name.clone()
}

fn strip_extension(path: &str) -> String {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    match path[name_start..].rfind('.') {
        Some(dot) if dot > 0 => path[..name_start + dot].to_owned(),
        _ => path.to_owned(),
    }
}

fn strip_code_extension(path: &str) -> String {
    const KNOWN: [&str; 10] = [
        "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "py", "pyi",
    ];
    match path.rsplit_once('.') {
        Some((stem, ext)) if KNOWN.contains(&ext) => stem.to_owned(),
        _ => path.to_owned(),
    }
}

/// Joins `relative` onto `base_dir`, resolving `.` and `..`.
fn join_path(base_dir: &[&str], relative: &str) -> String {
    let mut parts: Vec<&str> = base_dir.to_vec();
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn directory_of(path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts
}

/// Repository-relative paths a relative import may refer to.
fn relative_targets(language: Language, from: &str, import: &str, names: &[String]) -> Vec<String> {
    let dir = directory_of(from);
    match language {
        Language::Python => {
            let dots = import.chars().take_while(|c| *c == '.').count();
            let rest = import.trim_start_matches('.').replace('.', "/");
            let mut base = dir;
            for _ in 1..dots {
                base.pop();
            }
            if rest.is_empty() {
                names.iter().map(|n| join_path(&base, n)).collect()
            } else {
                vec![join_path(&base, &rest)]
            }
        }
        Language::C => vec![join_path(&dir, import)],
        l if l.is_js_family() => vec![strip_code_extension(&join_path(&dir, import))],
        // Rust module paths do not map to files without Cargo knowledge; their names are
        // linked as symbol references instead.
        _ => Vec::new(),
    }
}

/// Short name of the external package an import refers to.
fn package_short_name(language: Language, path: &str) -> Option<String> {
    let name = match language {
        Language::Rust | Language::Python => path.split(['.', ':']).find(|s| !s.is_empty())?,
        Language::Go => path.rsplit('/').find(|s| !s.is_empty())?,
        Language::Java => {
            let segments: Vec<&str> = path.split('.').collect();
            if segments.len() > 1 {
                segments[segments.len() - 2]
            } else {
                segments[0]
            }
        }
        Language::C => {
            let file = path.rsplit('/').next()?;
            file.split('.').next()?
        }
        _ => {
            let path = path.strip_prefix("node:").unwrap_or(path);
            let mut parts = path.split('/');
            let first = parts.next()?;
            if first.starts_with('@') {
                parts.next().unwrap_or(first)
            } else {
                first
            }
        }
    };
    (!name.is_empty()).then(|| name.to_owned())
}
