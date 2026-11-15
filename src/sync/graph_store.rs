//! [`UnitStore`] over the engine's [`GraphStore`].

use std::sync::Arc;

use crate::base::{Error, Result, SourceDescriptor, SourceId, Unit, UnitKey, UnitMeta};
use crate::engine::store::{ApplyOutcome, GraphStore};

use super::pack::PackEntry;
use super::store::{ApplyReport, Rejection, UnitStore, should_apply};

/// Called after every unit applied from a peer, so the engine can keep its in-memory graph and
/// vector index in lockstep (`engine.absorb`).
pub type OnApplied = Arc<dyn Fn(&ApplyOutcome) + Send + Sync>;

/// A [`UnitStore`] backed by a [`GraphStore`]. Local trust wins: a registered source keeps its
/// trust level; unknown sources are registered from the peer's descriptor.
pub struct GraphUnitStore {
    store: GraphStore,
    on_applied: OnApplied,
}

impl GraphUnitStore {
    /// Wraps `store`; `on_applied` runs after each applied unit.
    pub fn new(store: GraphStore, on_applied: OnApplied) -> Self {
        Self { store, on_applied }
    }
}

impl UnitStore for GraphUnitStore {
    fn sources(&self) -> Result<Vec<SourceDescriptor>> {
        self.store.sources()
    }

    fn metas(&self, source: &SourceId) -> Result<Vec<UnitMeta>> {
        self.store.unit_metas(source)
    }

    fn get(&self, key: &UnitKey) -> Result<Option<(UnitMeta, Option<Unit>)>> {
        let Some(meta) = self.store.unit_meta(key)? else {
            return Ok(None);
        };
        let unit = if meta.deleted {
            None
        } else {
            self.store.unit(key)?
        };
        Ok(Some((meta, unit)))
    }

    fn apply(&self, sources: &[SourceDescriptor], entries: Vec<PackEntry>) -> Result<ApplyReport> {
        let mut known: Vec<SourceId> = self.store.sources()?.into_iter().map(|s| s.id).collect();
        for source in sources {
            if !known.contains(&source.id) {
                self.store.upsert_source(source)?;
                known.push(source.id.clone());
            }
        }
        let mut report = ApplyReport::default();
        for mut entry in entries {
            let key = entry.meta.key.clone();
            let reject = |reason: String| Rejection {
                key: key.clone(),
                reason,
            };
            if !known.contains(&entry.meta.source) {
                report
                    .rejected
                    .push(reject(format!("unknown source `{}`", entry.meta.source)));
                continue;
            }
            if let Err(err) = entry.validate() {
                report.rejected.push(reject(err.to_string()));
                continue;
            }
            let existing = self.store.unit_meta(&entry.meta.key)?;
            if existing
                .as_ref()
                .is_some_and(|meta| meta.source != entry.meta.source)
            {
                report.rejected.push(reject(format!(
                    "unit belongs to another source than `{}`",
                    entry.meta.source
                )));
                continue;
            }
            if !should_apply(existing.as_ref(), &entry.meta) {
                report.stale += 1;
                continue;
            }
            match self.store.apply_remote(&entry.meta, entry.unit.as_ref()) {
                Ok(outcome) => {
                    (self.on_applied)(&outcome);
                    report.applied.push(entry.meta);
                }
                Err(err @ (Error::Integrity(_) | Error::Invalid(_))) => {
                    report.rejected.push(reject(err.to_string()));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(report)
    }
}
