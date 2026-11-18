//! Links between wiki pages found by their titles, without a model.
//!
//! A page's own ingest cannot know the titles of the other pages, so this runs over the whole
//! corpus after a full scan: every wiki page title (as written, at least [`MIN_TITLE_CHARS`]
//! characters, starting with a capital letter or digit) is matched case-sensitively, on word
//! boundaries, against the text of every other page. Each match becomes a `Mentions` edge,
//! recorded as one unit per linking page of the built-in `anchor-title-links` source
//! (`reference_only`, like every derived fact), so the links are replaced as pages change and
//! disappear with the pages they point to.
//!
//! A title shared by more than [`MAX_PAGES_PER_TITLE`] pages is too generic to mean one page
//! and is skipped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use aho_corasick::{AhoCorasick, MatchKind};

use super::core::{Engine, ScanReport};
use crate::base::{
    EdgeAssertion, EdgeKind, Error, NodeAssertion, NodeKey, NodeKind, Result, SourceDescriptor,
    SourceId, SourceKind, TrustLevel, Unit, UnitKind, keys,
};

/// Id of the built-in source that holds title links.
pub const TITLE_LINK_SOURCE: &str = "anchor-title-links";
/// Shortest title that is matched.
pub const MIN_TITLE_CHARS: usize = 4;
/// Titles naming more pages than this are not linked.
pub const MAX_PAGES_PER_TITLE: usize = 3;
/// Most links recorded from one page.
const MAX_LINKS_PER_PAGE: usize = 24;
/// Edge weight: an exact title is stronger evidence than a model's guess (0.6), weaker than an
/// explicit link (0.9).
const WEIGHT: f32 = 0.75;
/// Units applied per write transaction.
const BATCH: usize = 512;

fn linkable(title: &str) -> bool {
    title.chars().count() >= MIN_TITLE_CHARS
        && title
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || c.is_ascii_digit())
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte ranges of `text` where a pattern matches on word boundaries, with the pattern index.
fn matches(matcher: &AhoCorasick, text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for hit in matcher.find_iter(text) {
        let before = text[..hit.start()].chars().next_back();
        let after = text[hit.end()..].chars().next();
        if !before.is_some_and(word_char) && !after.is_some_and(word_char) {
            found.push(hit.pattern().as_usize());
        }
    }
    found
}

impl Engine {
    /// Rebuilds the title links of every wiki page. Unchanged pages cost a hash comparison.
    pub fn link_titles(&self) -> Result<ScanReport> {
        let started = Instant::now();
        let store = self.store();
        let source = SourceId::new(TITLE_LINK_SOURCE);
        let mut report = ScanReport {
            source: TITLE_LINK_SOURCE.to_owned(),
            depth: "derived".to_owned(),
            ..ScanReport::default()
        };
        let pages = store.nodes_by_kind(NodeKind::WikiPage, usize::MAX >> 1, 0)?;

        let mut by_title: BTreeMap<&str, Vec<&NodeKey>> = BTreeMap::new();
        for page in &pages {
            let title = page.name.trim();
            if linkable(title) {
                by_title.entry(title).or_default().push(&page.key);
            }
        }
        by_title.retain(|_, keys| keys.len() <= MAX_PAGES_PER_TITLE);
        let titles: Vec<&str> = by_title.keys().copied().collect();
        let targets: Vec<&Vec<&NodeKey>> = by_title.values().collect();

        let mut units = Vec::new();
        if !titles.is_empty() {
            store.upsert_source(&SourceDescriptor {
                id: source.clone(),
                kind: SourceKind::Derived,
                name: "Links between pages that name each other's titles".to_owned(),
                trust: TrustLevel::ReferenceOnly,
            })?;
            let matcher = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&titles)
                .map_err(|err| Error::other(format!("title matcher: {err}")))?;
            for page in &pages {
                report.files_seen += 1;
                let mut linked: BTreeSet<&NodeKey> = BTreeSet::new();
                for chunk in store.chunks_of(page.id)? {
                    // Only the page's own text: derived chunks (model summaries) are not evidence.
                    if chunk.source.as_str() == TITLE_LINK_SOURCE
                        || chunk.source.as_str() == super::enrich::ENRICHMENT_SOURCE
                    {
                        continue;
                    }
                    for pattern in matches(&matcher, &chunk.text) {
                        linked.extend(targets[pattern].iter().copied());
                    }
                }
                linked.remove(&page.key);
                if linked.is_empty() {
                    continue;
                }
                let mut unit = Unit::new(
                    keys::unit(&source, UnitKind::Links, page.key.as_str()),
                    source.clone(),
                    UnitKind::Links,
                );
                let from =
                    unit.add_node(NodeAssertion::new(page.kind, page.key.clone(), &page.name));
                for target in linked.into_iter().take(MAX_LINKS_PER_PAGE) {
                    unit.add_edge(
                        EdgeAssertion::new(&from, EdgeKind::Mentions, target.clone())
                            .weight(WEIGHT),
                    );
                }
                units.push(unit.finish());
            }
        }

        // Pages that no longer link to anything lose their unit.
        let produced: HashMap<&str, ()> = units.iter().map(|u| (u.key.as_str(), ())).collect();
        let stale: Vec<_> = store
            .unit_metas(&source)?
            .into_iter()
            .filter(|meta| !meta.deleted && !produced.contains_key(meta.key.as_str()))
            .map(|meta| meta.key)
            .collect();

        for batch in units.chunks(BATCH) {
            let outcome = self.apply(batch)?;
            if outcome.changed {
                report.units_applied += batch.len();
            }
            report.nodes_upserted += outcome.delta.nodes_upserted.len();
            report.edges_upserted += outcome.delta.edges_upserted.len();
        }
        if !stale.is_empty() {
            self.remove(&stale)?;
            report.units_removed = stale.len();
        }
        report.duration_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_match_whole_words_only() {
        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(["Paris", "Paris Hilton", "Rome"])
            .unwrap();
        assert_eq!(
            matches(
                &matcher,
                "Paris Hilton visited Rome, not Parisian Romefield."
            ),
            vec![1, 2]
        );
        assert_eq!(matches(&matcher, "(Paris)"), vec![0]);
        assert!(matches(&matcher, "paris").is_empty(), "case-sensitive");
        assert!(linkable("Rome") && linkable("1984"));
        assert!(!linkable("Ro") && !linkable("lowercase title"));
    }
}
