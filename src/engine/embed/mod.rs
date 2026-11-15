//! Local embeddings and the vector index.
//!
//! * [`HashEmbedder`]: deterministic feature hashing, the zero-download default.
//! * [`CandleEmbedder`] (cargo feature `embed-candle`): a BERT-family sentence-embedding model run
//!   in-process; its files are fetched on explicit request with [`ensure_model`].
//! * [`OllamaEmbedder`]: an embedding model served by Ollama.
//! * [`VectorIndex`]: int8-quantised vectors keyed by [`crate::base::ChunkId`], persisted in SQLite
//!   and searched exactly by dot product.
//! * [`index_chunks`]: embeds chunk texts in batches and stores the vectors that are missing.
//!
//! [`build_embedder`] turns configuration into an embedder.

#[cfg(feature = "embed-candle")]
mod candle;
mod config;
mod download;
mod hash;
mod index;
mod indexer;
mod ollama;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::base::{EmbedKind, Embedder, Error, Migration, Result};
use crate::config::{AnchorPaths, OllamaConfig};

#[cfg(feature = "embed-candle")]
pub use self::candle::{CandleEmbedder, MAX_TOKENS};
pub use config::{EmbeddingBackend, EmbeddingConfig};
pub use download::{DownloadProgress, MODEL_FILES, ensure_model, model_dir, model_files_present};
pub use hash::{HashEmbedder, tokenize};
pub use index::{MAX_DIMENSIONS, SearchHit, VectorFilter, VectorIndex};
pub use indexer::{IndexReport, index_chunks};
pub use ollama::{DEFAULT_OLLAMA_EMBED_MODEL, OllamaEmbedder};

/// Sentence-embedding model used by the `candle` backend when `embeddings.model` is empty.
pub const DEFAULT_CANDLE_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Schema history of the embedding tables.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "vector index",
    // One row per chunk: a chunk re-embedded under another model replaces its old vector. `scale`
    // is the per-vector dequantisation factor (largest absolute component / 127).
    sql: "CREATE TABLE embed_vectors (
              chunk_id     INTEGER PRIMARY KEY,
              model_id     TEXT    NOT NULL,
              content_hash BLOB    NOT NULL,
              dimensions   INTEGER NOT NULL,
              scale        REAL    NOT NULL,
              vector       BLOB    NOT NULL
          );
          CREATE INDEX embed_vectors_model ON embed_vectors (model_id);",
}];

/// Builds the embedder selected by `config`.
///
/// * `hash` always works (it only fails for a nonsensical `dimensions`).
/// * `candle` fails with [`Error::Config`] when the crate was built without the `embed-candle`
///   feature or when the model files are not in `paths.models_dir()`; nothing is ever downloaded
///   here. The caller decides whether to fall back to the hash embedder or to call
///   [`ensure_model`]. Loading reads the weights from disk, so call this off the async executor.
/// * `ollama` only validates the base URL; the server is first contacted when text is embedded.
pub fn build_embedder(
    config: &EmbeddingConfig,
    paths: &AnchorPaths,
    ollama: &OllamaConfig,
) -> Result<Arc<dyn Embedder>> {
    let batch_size = config.batch_size.max(1);
    match config.backend {
        EmbeddingBackend::Hash => Ok(Arc::new(HashEmbedder::new(config.dimensions)?)),
        EmbeddingBackend::Candle => {
            let repo = model_or(&config.model, DEFAULT_CANDLE_MODEL);
            build_candle(&paths.models_dir(), repo, batch_size)
        }
        EmbeddingBackend::Ollama => {
            let model = model_or(&config.model, DEFAULT_OLLAMA_EMBED_MODEL);
            Ok(Arc::new(OllamaEmbedder::new(
                &ollama.base_url,
                model,
                batch_size,
            )?))
        }
    }
}

fn model_or<'a>(configured: &'a str, default: &'a str) -> &'a str {
    let trimmed = configured.trim();
    if trimmed.is_empty() { default } else { trimmed }
}

#[cfg(feature = "embed-candle")]
fn build_candle(models_dir: &Path, repo: &str, batch_size: usize) -> Result<Arc<dyn Embedder>> {
    let dir = checked_model_dir(models_dir, repo)?;
    Ok(Arc::new(CandleEmbedder::load(&dir, repo, batch_size)?))
}

#[cfg(not(feature = "embed-candle"))]
fn build_candle(_models_dir: &Path, repo: &str, _batch_size: usize) -> Result<Arc<dyn Embedder>> {
    Err(Error::config(format!(
        "embeddings.backend = \"candle\" (model {repo}) needs an Anchor build with the \
         `embed-candle` cargo feature; rebuild with it or set embeddings.backend to \"hash\" or \
         \"ollama\""
    )))
}

/// Directory of `repo` under `models_dir`, or an actionable error when its files are missing.
#[cfg_attr(not(feature = "embed-candle"), allow(dead_code))]
fn checked_model_dir(models_dir: &Path, repo: &str) -> Result<PathBuf> {
    let dir = model_dir(models_dir, repo)?;
    if model_files_present(&dir) {
        return Ok(dir);
    }
    Err(Error::config(format!(
        "embedding model {repo} is not installed: expected {} in {}. Download it explicitly \
         (engine::embed::ensure_model), copy the files there by hand, or set embeddings.backend \
         to \"hash\"",
        MODEL_FILES.join(", "),
        dir.display()
    )))
}

/// Vector length of `embedder`, probing it with one short text when it does not know yet.
///
/// [`OllamaEmbedder`] learns its dimensions from the first response, so its
/// [`Embedder::dimensions`] is `0` until then. Call this before sizing a [`VectorIndex`].
pub async fn resolve_dimensions(embedder: &dyn Embedder) -> Result<usize> {
    let known = embedder.dimensions();
    if known > 0 {
        return Ok(known);
    }
    let probe = embedder
        .embed(&["dimension probe".to_owned()], EmbedKind::Query)
        .await?;
    match probe.first().map(Vec::len) {
        Some(len) if len > 0 => Ok(len),
        _ => Err(Error::other(format!(
            "embedder {} returned no vector for the dimension probe",
            embedder.model_id()
        ))),
    }
}

/// Scales `vector` to unit length in place. Zero and non-finite vectors are left untouched.
pub(crate) fn l2_normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    if norm.is_finite() && norm > 0.0 {
        let inverse = (1.0 / norm) as f32;
        for value in vector.iter_mut() {
            *value *= inverse;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, AnchorPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = AnchorPaths::new(dir.path());
        (dir, paths)
    }

    #[tokio::test]
    async fn hash_backend_always_builds() {
        let (_dir, paths) = paths();
        let embedder = build_embedder(
            &EmbeddingConfig::default(),
            &paths,
            &OllamaConfig::default(),
        )
        .unwrap();
        assert_eq!(embedder.model_id(), "hash-v1-384");
        assert_eq!(resolve_dimensions(embedder.as_ref()).await.unwrap(), 384);
        let zero = EmbeddingConfig {
            dimensions: 0,
            ..EmbeddingConfig::default()
        };
        assert!(build_embedder(&zero, &paths, &OllamaConfig::default()).is_err());
    }

    #[test]
    fn candle_backend_without_model_files_is_an_actionable_error() {
        let (_dir, paths) = paths();
        let config = EmbeddingConfig {
            backend: EmbeddingBackend::Candle,
            ..EmbeddingConfig::default()
        };
        let Err(err) = build_embedder(&config, &paths, &OllamaConfig::default()) else {
            panic!("candle without model files must fail");
        };
        let message = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{message}");
        assert!(message.contains("all-MiniLM-L6-v2"), "{message}");
        assert!(message.contains("hash"), "{message}");
    }

    #[test]
    fn ollama_backend_builds_without_contacting_the_server() {
        let (_dir, paths) = paths();
        let config = EmbeddingConfig {
            backend: EmbeddingBackend::Ollama,
            ..EmbeddingConfig::default()
        };
        let embedder = build_embedder(&config, &paths, &OllamaConfig::default()).unwrap();
        assert_eq!(embedder.model_id(), "ollama:nomic-embed-text");
        assert_eq!(embedder.dimensions(), 0);
    }

    #[test]
    fn normalisation_handles_zero_vectors() {
        let mut zero = vec![0.0_f32; 4];
        l2_normalize(&mut zero);
        assert_eq!(zero, vec![0.0; 4]);
        let mut v = vec![3.0_f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }
}
