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
//! and is skipped. A title that many pages name (a "Music" or "Payments API" every other page
//! mentions) still links, but more weakly: the weight is scaled by
//! `min(1, COMMON_TITLE_PAGES / pages naming it)`, so relevance spread at query time flows mostly
//! through specific links, the way inverse document frequency weights rare words.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use aho_corasick::{AhoCorasick, MatchKind};

use super::core::{Engine, ScanReport};
use super::store::NodeRecord;
use crate::base::{
    EdgeAssertion, EdgeKind, Error, NodeAssertion, NodeId, NodeKey, NodeKind, Result,
    SourceDescriptor, SourceId, SourceKind, TrustLevel, Unit, UnitKind, keys,
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
/// A title named by more pages than this links with proportionally less weight.
pub const COMMON_TITLE_PAGES: usize = 10;
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

/// Every linkable title, the pages each names, and a matcher over them.
struct TitleIndex {
    /// Title and the pages it names, in pattern order.
    patterns: Vec<(String, Vec<NodeKey>)>,
    matcher: Option<AhoCorasick>,
}

impl TitleIndex {
    fn new(pages: &[NodeRecord]) -> Result<Self> {
        let mut by_title: BTreeMap<String, Vec<NodeKey>> = BTreeMap::new();
        for page in pages {
            let title = page.name.trim();
            if linkable(title) {
                by_title
                    .entry(title.to_owned())
                    .or_default()
                    .push(page.key.clone());
            }
        }
        by_title.retain(|_, keys| keys.len() <= MAX_PAGES_PER_TITLE);
        let patterns: Vec<(String, Vec<NodeKey>)> = by_title.into_iter().collect();
        let matcher = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostLongest)
                    .build(patterns.iter().map(|(title, _)| title.as_str()))
                    .map_err(|err| Error::other(format!("title matcher: {err}")))?,
            )
        };
        Ok(Self { patterns, matcher })
    }
}

impl Engine {
    /// Rebuilds the title links of every wiki page. Unchanged pages cost a hash comparison.
    pub fn link_titles(&self) -> Result<ScanReport> {
        let pages = self
            .store()
            .nodes_by_kind(NodeKind::WikiPage, usize::MAX >> 1, 0)?;
        let all: Vec<&NodeRecord> = pages.iter().collect();
        let report = self.relink(&pages, &all, true)?;
        // Everything is current: nothing is left for an incremental pass.
        self.take_touched_pages();
        Ok(report)
    }

    /// Brings title links up to date after the pages in `touched` changed: their own links, and
    /// the links of every page whose text names one of their titles. Much cheaper than
    /// [`Self::link_titles`] when few pages changed. A page that disappears takes its links with
    /// it; links that pointed to it by a title nobody holds any more are dropped by the next full
    /// pass.
    pub fn relink_titles(&self, touched: &[NodeId]) -> Result<ScanReport> {
        let store = self.store();
        let pages = store.nodes_by_kind(NodeKind::WikiPage, usize::MAX >> 1, 0)?;
        let touched: BTreeSet<NodeId> = touched.iter().copied().collect();
        let mut affected: BTreeSet<NodeId> = BTreeSet::new();
        for page in pages.iter().filter(|page| touched.contains(&page.id)) {
            affected.insert(page.id);
            let title = page.name.trim();
            if linkable(title) {
                affected.extend(store.nodes_with_phrase(title, 1000)?);
            }
        }
        let subset: Vec<&NodeRecord> = pages.iter().filter(|p| affected.contains(&p.id)).collect();
        self.relink(&pages, &subset, false)
    }

    /// Recomputes the link units of `subset` against the titles of all `pages`. With `complete`,
    /// link units of pages outside `subset` are removed as well.
    fn relink(
        &self,
        pages: &[NodeRecord],
        subset: &[&NodeRecord],
        complete: bool,
    ) -> Result<ScanReport> {
        let started = Instant::now();
        let store = self.store();
        let source = SourceId::new(TITLE_LINK_SOURCE);
        let mut report = ScanReport {
            source: TITLE_LINK_SOURCE.to_owned(),
            depth: "derived".to_owned(),
            ..ScanReport::default()
        };
        let index = TitleIndex::new(pages)?;
        let mut units = Vec::new();
        if let Some(matcher) = &index.matcher {
            store.upsert_source(&SourceDescriptor {
                id: source.clone(),
                kind: SourceKind::Derived,
                name: "Links between pages that name each other's titles".to_owned(),
                trust: TrustLevel::ReferenceOnly,
            })?;
            let mut named: Vec<(&NodeRecord, BTreeSet<usize>)> = Vec::new();
            for page in subset {
                report.files_seen += 1;
                let mut found = BTreeSet::new();
                for chunk in store.chunks_of(page.id)? {
                    // Only the page's own text: derived chunks (model summaries) are not evidence.
                    if chunk.source.as_str() == TITLE_LINK_SOURCE
                        || chunk.source.as_str() == super::enrich::ENRICHMENT_SOURCE
                    {
                        continue;
                    }
                    found.extend(matches(matcher, &chunk.text));
                }
                named.push((page, found));
            }
            // How many pages name each title: counted here on a full pass, by a phrase search
            // when only some pages are recomputed.
            let mut naming: HashMap<usize, usize> = HashMap::new();
            if complete {
                for (page, found) in &named {
                    for pattern in found {
                        // A page's own heading does not count as naming it.
                        if !index.patterns[*pattern].1.contains(&page.key) {
                            *naming.entry(*pattern).or_default() += 1;
                        }
                    }
                }
            } else {
                let found: BTreeSet<usize> =
                    named.iter().flat_map(|(_, f)| f.iter().copied()).collect();
                for pattern in found {
                    let count = store
                        .nodes_with_phrase(&index.patterns[pattern].0, COMMON_TITLE_PAGES * 100)?
                        .len();
                    // The pages holding the title contain it in their own heading.
                    let own = index.patterns[pattern].1.len();
                    naming.insert(pattern, count.saturating_sub(own));
                }
            }
            for (page, found) in named {
                let mut linked: BTreeMap<&NodeKey, f32> = BTreeMap::new();
                for pattern in &found {
                    let pages = naming.get(pattern).copied().unwrap_or(1).max(1);
                    let weight = WEIGHT * (COMMON_TITLE_PAGES as f32 / pages as f32).min(1.0);
                    for key in &index.patterns[*pattern].1 {
                        let w = linked.entry(key).or_insert(0.0);
                        *w = w.max(weight);
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
                for (target, weight) in linked.into_iter().take(MAX_LINKS_PER_PAGE) {
                    unit.add_edge(
                        EdgeAssertion::new(&from, EdgeKind::Mentions, target.clone())
                            .weight(weight),
                    );
                }
                units.push(unit.finish());
            }
        }

        // Pages that no longer link to anything lose their unit.
        let produced: HashSet<&str> = units.iter().map(|u| u.key.as_str()).collect();
        let considered: HashSet<String> = subset
            .iter()
            .map(|page| {
                keys::unit(&source, UnitKind::Links, page.key.as_str())
                    .as_str()
                    .to_owned()
            })
            .collect();
        let stale: Vec<_> = store
            .unit_metas(&source)?
            .into_iter()
            .filter(|meta| {
                !meta.deleted
                    && !produced.contains(meta.key.as_str())
                    && (complete || considered.contains(meta.key.as_str()))
            })
            .map(|meta| meta.key)
            .collect();

        for batch in units.chunks(BATCH) {
            let outcome = self.apply(batch)?;
            report.units_applied += outcome.units_changed;
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
