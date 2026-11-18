//! Graph traversal configuration (`[graph]`).

use serde::{Deserialize, Serialize};

/// How trust levels translate into traversal cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustWeights {
    /// Cost multiplier applied to `SourceOfTruth` nodes and edges. Low: authoritative data is cheap
    /// to traverse.
    pub source_of_truth_multiplier: f32,
    /// Cost multiplier applied to `ReferenceOnly` nodes and edges. High: contextual material is
    /// heavily penalised.
    pub reference_only_multiplier: f32,
    /// When `true`, a path that uses fewer `ReferenceOnly` elements always beats a path that uses
    /// more, regardless of length, so authoritative data wins every conflict. When `false`, only
    /// the multipliers apply.
    pub strict_authority: bool,
}

impl Default for TrustWeights {
    fn default() -> Self {
        Self {
            source_of_truth_multiplier: 1.0,
            reference_only_multiplier: 8.0,
            strict_authority: true,
        }
    }
}

/// Limits and weights of graph traversal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Trust weighting.
    pub trust: TrustWeights,
    /// Maximum number of edges on a path.
    pub max_hops: u32,
    /// Maximum total weighted cost of a path.
    pub max_cost: f32,
    /// Maximum number of nodes a single search visits. Searches that hit it stop early and the
    /// answer is marked as truncated.
    pub max_visits: usize,
    /// After a full scan, link wiki pages to the other pages whose titles they name (see
    /// [`crate::engine::title_links`]).
    pub title_links: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            trust: TrustWeights::default(),
            max_hops: 6,
            max_cost: 64.0,
            max_visits: 200_000,
            title_links: true,
        }
    }
}
