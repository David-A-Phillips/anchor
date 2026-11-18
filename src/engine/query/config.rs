//! Query engine configuration (`[query]`).

use serde::{Deserialize, Serialize};

/// What to do when a question touches a source that is ingested below full depth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShallowPolicy {
    /// Deepen the source first, then answer from the deeper graph.
    Block,
    /// Answer now from what is there, flag it, and deepen in the background.
    #[default]
    Background,
    /// Answer now, flag it, and wait for an explicit `anchor deepen`.
    Manual,
}

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
    /// Let a model write the answer when the graph cannot answer directly. Off by default:
    /// answers come straight from the graph in milliseconds; `anchor query --explain` asks a
    /// model for prose on demand.
    pub model_answers: bool,
    /// Behaviour when a question touches a shallow source.
    pub on_shallow: ShallowPolicy,
    /// Time budget of graph retrieval for one question, in milliseconds. Traversal stops when
    /// it runs out and the answer is marked `truncated`.
    pub timeout_ms: u64,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            top_k: 12,
            max_context_tokens: 6000,
            confidence_threshold: 0.55,
            model_answers: false,
            on_shallow: ShallowPolicy::Background,
            timeout_ms: 10_000,
        }
    }
}
