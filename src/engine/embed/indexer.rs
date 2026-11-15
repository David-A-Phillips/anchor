//! Batch indexing: embed the chunks whose vectors are missing or outdated.

use crate::base::{ChunkId, ContentHash, EmbedKind, Embedder, Error, Result};

use super::index::VectorIndex;

/// Outcome of [`index_chunks`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// Chunks embedded and stored.
    pub embedded: usize,
    /// Chunks already indexed under the current model with the same content hash.
    pub skipped: usize,
    /// Chunks that could not be embedded (embedder error, or an unusable vector such as the zero
    /// vector of an empty text). They stay absent from the index and are retried on the next run.
    pub failed: usize,
    /// Message of the first failure, for diagnostics.
    pub first_error: Option<String>,
}

impl IndexReport {
    fn fail(&mut self, count: usize, message: impl FnOnce() -> String) {
        self.failed += count;
        if self.first_error.is_none() {
            self.first_error = Some(message());
        }
    }
}

/// Embeds `chunks` (`(id, content hash, text)`) in batches of `batch_size` and stores the vectors
/// in `index`, skipping chunks that [`VectorIndex::contains`] with the same hash.
///
/// A failing batch (unreachable embedding service, malformed response) is counted as failed and
/// the run continues with the next batch, so one bad batch does not block the rest. Hard errors
/// are returned: an embedder whose model id or dimensions differ from the index's, and database
/// failures.
pub async fn index_chunks<I>(
    embedder: &dyn Embedder,
    index: &VectorIndex,
    chunks: I,
    batch_size: usize,
) -> Result<IndexReport>
where
    I: IntoIterator<Item = (ChunkId, ContentHash, String)>,
    I::IntoIter: Send,
{
    if embedder.model_id() != index.model_id() {
        return Err(Error::invalid(format!(
            "embedder {} cannot fill the vector index of model {}",
            embedder.model_id(),
            index.model_id()
        )));
    }
    let known = embedder.dimensions();
    if known != 0 && known != index.dimensions() {
        return Err(Error::invalid(format!(
            "embedder {} produces {known} dimensions, the vector index has {}",
            embedder.model_id(),
            index.dimensions()
        )));
    }

    let batch_size = batch_size.max(1);
    let mut report = IndexReport::default();
    let mut pending: Vec<(ChunkId, ContentHash, String)> = Vec::with_capacity(batch_size);
    let mut chunks = chunks.into_iter();
    loop {
        let next = chunks.next();
        let exhausted = next.is_none();
        if let Some((id, hash, text)) = next {
            if index.contains(id, &hash) {
                report.skipped += 1;
            } else {
                pending.push((id, hash, text));
            }
        }
        if pending.len() >= batch_size || (exhausted && !pending.is_empty()) {
            let batch = std::mem::take(&mut pending);
            embed_batch(embedder, index, batch, &mut report).await?;
        }
        if exhausted {
            return Ok(report);
        }
    }
}

async fn embed_batch(
    embedder: &dyn Embedder,
    index: &VectorIndex,
    batch: Vec<(ChunkId, ContentHash, String)>,
    report: &mut IndexReport,
) -> Result<()> {
    let mut ids = Vec::with_capacity(batch.len());
    let mut texts = Vec::with_capacity(batch.len());
    for (id, hash, text) in batch {
        ids.push((id, hash));
        texts.push(text);
    }
    let vectors = match embedder.embed(&texts, EmbedKind::Document).await {
        Ok(vectors) if vectors.len() == texts.len() => vectors,
        Ok(vectors) => {
            report.fail(texts.len(), || {
                format!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    texts.len()
                )
            });
            return Ok(());
        }
        Err(err) => {
            tracing::warn!(error = %err, chunks = texts.len(), "embedding batch failed");
            report.fail(texts.len(), || err.to_string());
            return Ok(());
        }
    };

    let dimensions = index.dimensions();
    let mut items = Vec::with_capacity(vectors.len());
    for ((id, hash), vector) in ids.into_iter().zip(vectors) {
        let usable = vector.len() == dimensions
            && vector.iter().all(|value| value.is_finite())
            && vector.iter().any(|value| *value != 0.0);
        if usable {
            items.push((id, hash, vector));
        } else {
            report.fail(1, || {
                format!(
                    "chunk {id}: unusable vector ({} dimensions, expected {dimensions}, or zero / \
                     non-finite)",
                    vector.len()
                )
            });
        }
    }
    let stored = items.len();
    index.upsert_many_async(items).await?;
    report.embedded += stored;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::base::Db;
    use crate::engine::embed::HashEmbedder;
    use crate::engine::embed::index::tests::SCHEMA;

    /// Hash embedder that counts calls and fails any batch containing the text "boom".
    struct Flaky {
        inner: HashEmbedder,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Embedder for Flaky {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if texts.iter().any(|text| text == "boom") {
                return Err(Error::other("service unavailable"));
            }
            self.inner.embed(texts, kind).await
        }
    }

    fn chunk(id: i64, text: &str) -> (ChunkId, ContentHash, String) {
        (
            ChunkId(id),
            ContentHash::of(text.as_bytes()),
            text.to_owned(),
        )
    }

    #[tokio::test]
    async fn embeds_missing_chunks_and_skips_known_ones() {
        let db = Db::open_in_memory_with(SCHEMA).unwrap();
        let embedder = Flaky {
            inner: HashEmbedder::new(64).unwrap(),
            calls: AtomicUsize::new(0),
        };
        let index = VectorIndex::load(&db, embedder.model_id(), 64).unwrap();
        let chunks: Vec<_> = (0..5)
            .map(|i| chunk(i, &format!("chunk number {i}")))
            .collect();

        let report = index_chunks(&embedder, &index, chunks.clone(), 2)
            .await
            .unwrap();
        assert_eq!((report.embedded, report.skipped, report.failed), (5, 0, 0));
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 3, "batches of two");
        assert_eq!(index.len(), 5);

        // Second run: one changed text, one failing batch, one empty text, the rest unchanged.
        let mut again = chunks;
        again[1] = chunk(1, "rewritten content");
        again.push(chunk(7, "boom"));
        again.push(chunk(8, "survives its neighbour's failure"));
        again.push(chunk(9, ""));
        let report = index_chunks(&embedder, &index, again, 1).await.unwrap();
        assert_eq!((report.embedded, report.skipped, report.failed), (2, 4, 2));
        assert!(report.first_error.unwrap().contains("service unavailable"));
        assert!(index.contains(ChunkId(1), &ContentHash::of(b"rewritten content")));
        assert!(index.contains(ChunkId(8), &chunk(8, "survives its neighbour's failure").1));
        assert_eq!(index.len(), 6);
    }

    #[tokio::test]
    async fn mismatched_model_or_dimensions_is_an_error() {
        let db = Db::open_in_memory_with(SCHEMA).unwrap();
        let embedder = HashEmbedder::new(64).unwrap();
        let other_model = VectorIndex::load(&db, "another-model", 64).unwrap();
        assert!(
            index_chunks(&embedder, &other_model, Vec::new(), 8)
                .await
                .is_err()
        );
        let other_size = VectorIndex::load(&db, embedder.model_id(), 32).unwrap();
        assert!(
            index_chunks(&embedder, &other_size, Vec::new(), 8)
                .await
                .is_err()
        );
    }
}
