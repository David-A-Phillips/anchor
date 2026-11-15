//! The storage seam of synchronisation: [`UnitStore`], an in-memory implementation, and the
//! per-namespace Merkle trees maintained over any store.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::base::{ContentHash, Result, SourceDescriptor, SourceId, Unit, UnitKey, UnitMeta};

use super::merkle::{MerkleNode, MerkleSource, MerkleTree, NodePath};
use super::pack::PackEntry;

/// A unit version the store refused, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    /// Key of the refused unit.
    pub key: UnitKey,
    /// Human-readable reason.
    pub reason: String,
}

/// The outcome of [`UnitStore::apply`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Metas of the versions that replaced what the store had (or were new to it).
    pub applied: Vec<UnitMeta>,
    /// Number of versions skipped because the store already had the same or a superseding one.
    pub stale: u64,
    /// Versions refused as invalid.
    pub rejected: Vec<Rejection>,
}

impl ApplyReport {
    /// Folds the outcome of a later batch into this one.
    pub fn merge(&mut self, later: ApplyReport) {
        self.applied.extend(later.applied);
        self.stale += later.stale;
        self.rejected.extend(later.rejected);
    }
}

/// The merge rule, in one place: `incoming` replaces `existing` exactly when there is nothing yet
/// or [`UnitMeta::supersedes`] says so. Every [`UnitStore`] must decide with this function, which
/// is what makes merging commutative, associative and idempotent.
pub fn should_apply(existing: Option<&UnitMeta>, incoming: &UnitMeta) -> bool {
    existing.is_none_or(|current| incoming.supersedes(current))
}

/// What synchronisation needs from the system of record. Blocking: call it from a blocking thread.
///
/// The engine's graph store implements this through a thin adapter; [`MemoryUnitStore`] serves
/// tests and relay-only servers.
pub trait UnitStore: Send + Sync {
    /// Every source (sync namespace) the store knows.
    fn sources(&self) -> Result<Vec<SourceDescriptor>>;

    /// The metas of all unit versions of `source`, tombstones included, in any order. An unknown
    /// source has none.
    fn metas(&self, source: &SourceId) -> Result<Vec<UnitMeta>>;

    /// The current version of the unit `key`: its meta and, unless it is a tombstone, the unit.
    fn get(&self, key: &UnitKey) -> Result<Option<(UnitMeta, Option<Unit>)>>;

    /// Merges remote unit versions.
    ///
    /// `sources` describes the namespaces the entries belong to. A source the store does not know
    /// yet is created from its descriptor; a descriptor for a known source is ignored, because the
    /// name and above all the trust level of a source are local decisions that a peer must not be
    /// able to change. Each entry is then validated ([`PackEntry::validate`]; its source must be
    /// known or described) and applied if [`should_apply`] holds. Invalid entries are reported in
    /// [`ApplyReport::rejected`]; only storage failures are errors.
    fn apply(&self, sources: &[SourceDescriptor], entries: Vec<PackEntry>) -> Result<ApplyReport>;
}

#[derive(Default)]
struct MemoryInner {
    sources: BTreeMap<SourceId, SourceDescriptor>,
    units: BTreeMap<UnitKey, (UnitMeta, Option<Unit>)>,
}

/// A [`UnitStore`] kept in memory.
#[derive(Default)]
pub struct MemoryUnitStore {
    inner: RwLock<MemoryInner>,
}

impl MemoryUnitStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates or replaces a source descriptor (local configuration, unlike [`UnitStore::apply`]).
    pub fn upsert_source(&self, source: SourceDescriptor) {
        self.inner.write().sources.insert(source.id.clone(), source);
    }

    /// Number of unit versions held, tombstones included.
    pub fn len(&self) -> usize {
        self.inner.read().units.len()
    }

    /// `true` when the store holds no unit version.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl UnitStore for MemoryUnitStore {
    fn sources(&self) -> Result<Vec<SourceDescriptor>> {
        Ok(self.inner.read().sources.values().cloned().collect())
    }

    fn metas(&self, source: &SourceId) -> Result<Vec<UnitMeta>> {
        Ok(self
            .inner
            .read()
            .units
            .values()
            .filter(|(meta, _)| &meta.source == source)
            .map(|(meta, _)| meta.clone())
            .collect())
    }

    fn get(&self, key: &UnitKey) -> Result<Option<(UnitMeta, Option<Unit>)>> {
        Ok(self.inner.read().units.get(key).cloned())
    }

    fn apply(&self, sources: &[SourceDescriptor], entries: Vec<PackEntry>) -> Result<ApplyReport> {
        let mut inner = self.inner.write();
        for source in sources {
            inner
                .sources
                .entry(source.id.clone())
                .or_insert_with(|| source.clone());
        }
        let mut report = ApplyReport::default();
        for mut entry in entries {
            let rejected_key = entry.meta.key.clone();
            let reject = move |reason: String| Rejection {
                key: rejected_key.clone(),
                reason,
            };
            if !inner.sources.contains_key(&entry.meta.source) {
                report
                    .rejected
                    .push(reject(format!("unknown source `{}`", entry.meta.source)));
                continue;
            }
            if let Err(err) = entry.validate() {
                report.rejected.push(reject(err.to_string()));
                continue;
            }
            let existing = inner.units.get(&entry.meta.key).map(|(meta, _)| meta);
            if existing.is_some_and(|meta| meta.source != entry.meta.source) {
                report.rejected.push(reject(format!(
                    "unit belongs to another source than `{}`",
                    entry.meta.source
                )));
                continue;
            }
            if should_apply(existing, &entry.meta) {
                report.applied.push(entry.meta.clone());
                inner
                    .units
                    .insert(entry.meta.key.clone(), (entry.meta, entry.unit));
            } else {
                report.stale += 1;
            }
        }
        Ok(report)
    }
}

/// The Merkle trees of every namespace of a [`UnitStore`].
///
/// Built once from the store, then kept current incrementally: feed it every meta the store
/// applies ([`Self::record`], [`Self::record_applied`]), whether it came from local ingestion or
/// from a peer. It is a plain value; share it behind a lock.
#[derive(Clone, Debug, Default)]
pub struct NamespaceTrees {
    trees: BTreeMap<SourceId, MerkleTree>,
}

impl NamespaceTrees {
    /// No namespaces.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the trees of every source of `store`. Blocking.
    pub fn build(store: &dyn UnitStore) -> Result<Self> {
        let mut trees = Self::new();
        for source in store.sources()? {
            trees.rebuild(store, &source.id)?;
        }
        Ok(trees)
    }

    /// Rebuilds the tree of one source from the store (for example after a bulk re-ingestion).
    pub fn rebuild(&mut self, store: &dyn UnitStore, source: &SourceId) -> Result<()> {
        let tree = MerkleTree::from_metas(store.metas(source)?);
        self.trees.insert(source.clone(), tree);
        Ok(())
    }

    /// Records that `meta` is now the current version of its unit.
    pub fn record(&mut self, meta: UnitMeta) {
        self.trees
            .entry(meta.source.clone())
            .or_default()
            .insert(meta);
    }

    /// Records everything a [`UnitStore::apply`] call applied.
    pub fn record_applied(&mut self, report: &ApplyReport) {
        for meta in &report.applied {
            self.record(meta.clone());
        }
    }

    /// Forgets a unit that was purged from the store altogether (not a deletion: those are
    /// tombstone metas and go through [`Self::record`]).
    pub fn forget(&mut self, source: &SourceId, key: &UnitKey) -> Option<UnitMeta> {
        self.trees.get_mut(source)?.remove(key)
    }

    /// The tree of `source`, if the namespace exists.
    pub fn tree(&self, source: &SourceId) -> Option<&MerkleTree> {
        self.trees.get(source)
    }

    /// Root of `source`; [`ContentHash::ZERO`] for an unknown or empty namespace.
    pub fn root(&self, source: &SourceId) -> ContentHash {
        self.trees
            .get(source)
            .map_or(ContentHash::ZERO, MerkleTree::root)
    }

    /// Roots of all namespaces.
    pub fn roots(&self) -> BTreeMap<SourceId, ContentHash> {
        self.trees
            .iter()
            .map(|(source, tree)| (source.clone(), tree.root()))
            .collect()
    }

    /// The namespaces, ordered.
    pub fn namespaces(&self) -> impl Iterator<Item = &SourceId> {
        self.trees.keys()
    }

    /// The nodes at `paths` of the tree of `namespace` (an empty tree when unknown): the answer to
    /// a peer's tree node request.
    pub fn nodes(&self, namespace: &SourceId, paths: &[NodePath]) -> Vec<MerkleNode> {
        match self.trees.get(namespace) {
            Some(tree) => paths.iter().map(|path| tree.node(path)).collect(),
            None => {
                let empty = MerkleTree::new();
                paths.iter().map(|path| empty.node(path)).collect()
            }
        }
    }
}

impl MerkleSource for &NamespaceTrees {
    fn fetch_nodes(&mut self, namespace: &SourceId, paths: &[NodePath]) -> Result<Vec<MerkleNode>> {
        Ok(self.nodes(namespace, paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::pack::tests::{dead, live, source};

    #[test]
    fn apply_follows_the_supersedes_rule() {
        let store = MemoryUnitStore::new();
        let sources = [source("alpha")];
        let v1 = live("alpha", "a.rs", "one", 1, "d1");
        let v2 = live("alpha", "a.rs", "two", 2, "d1");
        let gone = dead("alpha", "a.rs", 3, "d2");

        let report = store
            .apply(&sources, vec![v2.clone(), v1.clone(), v2.clone()])
            .expect("apply");
        assert_eq!(report.applied, vec![v2.meta.clone()]);
        assert_eq!(report.stale, 2);
        let (meta, unit) = store.get(&v2.meta.key).expect("get").expect("present");
        assert_eq!((meta, unit), (v2.meta.clone(), v2.unit.clone()));

        // A tombstone supersedes, stays in the listing, and resists older live versions.
        let report = store
            .apply(&[], vec![gone.clone(), v2.clone()])
            .expect("apply");
        assert_eq!((report.applied.len(), report.stale), (1, 1));
        let (meta, unit) = store.get(&gone.meta.key).expect("get").expect("present");
        assert!(meta.deleted && unit.is_none());
        assert_eq!(
            store.metas(&sources[0].id).expect("metas"),
            vec![gone.meta.clone()]
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn apply_rejects_invalid_entries_and_keeps_local_descriptors() {
        let store = MemoryUnitStore::new();
        let mut local = source("alpha");
        local.name = "Local name".to_owned();
        store.upsert_source(local.clone());

        let mut corrupt = live("alpha", "a.rs", "one", 1, "d1");
        corrupt.meta.hash = ContentHash::of(b"other");
        let unknown = live("gamma", "g.rs", "one", 1, "d1");
        let mut moved = live("alpha", "b.rs", "one", 5, "d1");
        let good = live("alpha", "b.rs", "one", 1, "d1");
        moved.meta.source = SourceId::new("beta");
        if let Some(unit) = &mut moved.unit {
            unit.source = SourceId::new("beta");
        }
        moved = PackEntry::live(moved.unit.clone().expect("unit"), moved.meta.version())
            .expect("entry");

        let report = store
            .apply(
                &[source("alpha"), source("beta")],
                vec![corrupt, unknown, good.clone(), moved],
            )
            .expect("apply");
        assert_eq!(report.applied, vec![good.meta]);
        assert_eq!(report.rejected.len(), 3);
        assert!(report.rejected[0].reason.contains("content hash"));
        assert!(report.rejected[1].reason.contains("unknown source"));
        assert!(report.rejected[2].reason.contains("another source"));
        let sources = store.sources().expect("sources");
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], local);
    }

    #[test]
    fn namespace_trees_track_the_store() {
        let store = MemoryUnitStore::new();
        let sources = [source("alpha"), source("beta")];
        let mut trees = NamespaceTrees::build(&store).expect("build");
        assert_eq!(trees.root(&sources[0].id), ContentHash::ZERO);

        let batch = vec![
            live("alpha", "a.rs", "one", 1, "d1"),
            live("alpha", "b.rs", "two", 1, "d1"),
            live("beta", "x.md", "three", 1, "d1"),
        ];
        trees.record_applied(&store.apply(&sources, batch).expect("apply"));
        trees.record_applied(
            &store
                .apply(&[], vec![dead("alpha", "a.rs", 2, "d1")])
                .expect("apply"),
        );
        let rebuilt = NamespaceTrees::build(&store).expect("build");
        assert_eq!(trees.roots(), rebuilt.roots());
        assert_eq!(trees.tree(&sources[0].id).expect("alpha").len(), 2);
        assert_ne!(trees.root(&sources[0].id), trees.root(&sources[1].id));

        let unknown = SourceId::new("nope");
        let nodes = trees.nodes(&unknown, &[NodePath::root()]);
        assert_eq!(nodes[0].hash, ContentHash::ZERO);
        let key = dead("alpha", "a.rs", 2, "d1").meta.key;
        assert!(trees.forget(&sources[0].id, &key).is_some());
        assert_eq!(trees.tree(&sources[0].id).expect("alpha").len(), 1);
    }
}
