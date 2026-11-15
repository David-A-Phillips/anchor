//! Query engine configuration (`[query]`).

use serde::{Deserialize, Serialize};

/// Retrieval and answer settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryConfig {
    /// Maximum number of context items returned for a query.
    pub top_k: usize,
    /// Token budget of the assembled context.
    pub max_context_tokens: usize,
    /// Queries answered with confidence below this value are logged to the backlog.
    pub confidence_threshold: f32,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            top_k: 12,
            max_context_tokens: 6000,
            confidence_threshold: 0.55,
        }
    }
}
