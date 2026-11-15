//! The text-embedding contract.

use async_trait::async_trait;

use super::Result;
use super::macros::string_enum;

string_enum! {
    /// What is being embedded. Some models use different prompts for the two sides of retrieval.
    pub enum EmbedKind {
        /// Stored text (chunks).
        Document = 1 => "document",
        /// A user query.
        Query = 2 => "query",
    }
}

/// Turns text into dense vectors.
///
/// Implementations return one L2-normalised vector of [`Embedder::dimensions`] floats per input,
/// in input order, so cosine similarity is a plain dot product.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Stable identifier of the model and its configuration (e.g. `"hash-v1-384"`,
    /// `"candle:sentence-transformers/all-MiniLM-L6-v2"`). Stored next to every vector; vectors
    /// produced under a different identifier are never compared and are re-embedded lazily.
    fn model_id(&self) -> &str;

    /// Length of the produced vectors.
    fn dimensions(&self) -> usize;

    /// Embeds a batch of texts. CPU-bound implementations must not block the async executor.
    async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>>;
}
