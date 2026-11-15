//! Embedding configuration (`[embeddings]`).

use serde::{Deserialize, Serialize};

/// Which embedder produces vectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingBackend {
    /// Deterministic feature hashing. No model download, no network; the always-available default.
    #[default]
    Hash,
    /// A sentence-embedding model run in-process with Candle (requires the `embed-candle` feature).
    Candle,
    /// An embedding model served by Ollama.
    Ollama,
}

/// Embedding settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Embedder to use.
    pub backend: EmbeddingBackend,
    /// Model identifier for the `candle` and `ollama` backends; empty selects the backend default.
    pub model: String,
    /// Vector length for the `hash` backend (model backends report their own).
    pub dimensions: usize,
    /// Number of texts embedded per batch.
    pub batch_size: usize,
    /// Also embed code chunks (signatures and body previews). Off by default: documentation,
    /// doc comments, comments and descriptions are embedded; code is found by name and full text.
    pub embed_code: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            backend: EmbeddingBackend::Hash,
            model: String::new(),
            dimensions: 384,
            batch_size: 32,
            embed_code: false,
        }
    }
}
