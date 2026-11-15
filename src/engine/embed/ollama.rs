//! Embeddings served by Ollama (`POST /api/embed`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use super::l2_normalize;
use crate::base::{EmbedKind, Embedder, Error, Result};
use crate::ollama::OllamaApi;

/// Embedding model used by the `ollama` backend when `embeddings.model` is empty.
pub const DEFAULT_OLLAMA_EMBED_MODEL: &str = "nomic-embed-text";

/// Deadline of one batch request. Generous: the first request may have to load the model.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// An embedding model served by an Ollama server.
///
/// The vector length is not known up front: [`Embedder::dimensions`] returns `0` until the first
/// successful [`Embedder::embed`] call (see [`super::resolve_dimensions`]) and the discovered
/// value afterwards. A later response of another length is an error, since such vectors could not
/// be compared with the stored ones.
#[derive(Debug)]
pub struct OllamaEmbedder {
    api: OllamaApi,
    model: String,
    model_id: String,
    batch_size: usize,
    dimensions: AtomicUsize,
    /// `nomic-embed-text` is trained with task prefixes and retrieves noticeably worse without.
    task_prefixes: bool,
}

impl OllamaEmbedder {
    /// An embedder for `model` on the server at `base_url`. Does not contact the server; fails
    /// only when `base_url` is malformed or `model` is empty.
    pub fn new(base_url: &str, model: &str, batch_size: usize) -> Result<Self> {
        Self::with_api(OllamaApi::new(base_url)?, model, batch_size)
    }

    /// Like [`Self::new`] with an existing API client.
    pub fn with_api(api: OllamaApi, model: &str, batch_size: usize) -> Result<Self> {
        let model = model.trim();
        if model.is_empty() {
            return Err(Error::config("the Ollama embedding model name is empty"));
        }
        Ok(Self {
            api,
            model: model.to_owned(),
            model_id: format!("ollama:{model}"),
            batch_size: batch_size.max(1),
            dimensions: AtomicUsize::new(0),
            task_prefixes: model.starts_with("nomic-embed-text"),
        })
    }

    fn check_dimensions(&self, found: usize) -> Result<()> {
        if found == 0 {
            return Err(Error::other(format!(
                "Ollama model {} returned an empty embedding",
                self.model
            )));
        }
        match self
            .dimensions
            .compare_exchange(0, found, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => Ok(()),
            Err(known) if known == found => Ok(()),
            Err(known) => Err(Error::other(format!(
                "Ollama model {} returned {found}-dimensional embeddings after \
                 {known}-dimensional ones",
                self.model
            ))),
        }
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions.load(Ordering::SeqCst)
    }

    async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let prefix = match kind {
            EmbedKind::Document => "search_document: ",
            EmbedKind::Query => "search_query: ",
        };
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.batch_size) {
            let response = if self.task_prefixes {
                let prefixed: Vec<String> =
                    batch.iter().map(|text| format!("{prefix}{text}")).collect();
                self.api
                    .embed(&self.model, &prefixed, Some(REQUEST_TIMEOUT))
                    .await?
            } else {
                self.api
                    .embed(&self.model, batch, Some(REQUEST_TIMEOUT))
                    .await?
            };
            for mut vector in response.embeddings {
                self.check_dimensions(vector.len())?;
                if vector.iter().any(|value| !value.is_finite()) {
                    return Err(Error::other(format!(
                        "Ollama model {} returned a non-finite embedding",
                        self.model
                    )));
                }
                l2_normalize(&mut vector);
                vectors.push(vector);
            }
        }
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::*;

    type Seen = Arc<Mutex<Vec<Vec<String>>>>;

    /// Fake `/api/embed`: `dims` unnormalised components derived from each input's length.
    async fn fake(dims: Arc<AtomicUsize>) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let recorded = seen.clone();
        let app = Router::new().route(
            "/api/embed",
            post(move |Json(body): Json<Value>| {
                let recorded = recorded.clone();
                let dims = dims.clone();
                async move {
                    let inputs: Vec<String> =
                        serde_json::from_value(body["input"].clone()).unwrap();
                    let n = dims.load(Ordering::SeqCst);
                    let embeddings: Vec<Vec<f32>> = inputs
                        .iter()
                        .map(|text| (0..n).map(|i| (text.len() + i) as f32).collect())
                        .collect();
                    recorded.lock().unwrap().push(inputs);
                    Json(json!({ "model": body["model"], "embeddings": embeddings }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn batches_normalises_and_discovers_dimensions() {
        let dims = Arc::new(AtomicUsize::new(6));
        let (url, seen) = fake(dims.clone()).await;
        let embedder = OllamaEmbedder::new(&url, "", 2).unwrap_err();
        assert!(matches!(embedder, Error::Config(_)));
        let embedder = OllamaEmbedder::new(&url, "all-minilm", 2).unwrap();
        assert_eq!(embedder.model_id(), "ollama:all-minilm");
        assert_eq!(embedder.dimensions(), 0);
        assert_eq!(
            crate::engine::embed::resolve_dimensions(&embedder)
                .await
                .unwrap(),
            6
        );
        seen.lock().unwrap().clear();

        let texts: Vec<String> = ["a", "bb", "ccc", "dddd", "eeeee"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let vectors = embedder.embed(&texts, EmbedKind::Document).await.unwrap();
        assert_eq!(vectors.len(), 5);
        assert_eq!(embedder.dimensions(), 6);
        for vector in &vectors {
            assert_eq!(vector.len(), 6);
            let norm: f32 = vector.iter().map(|v| v * v).sum();
            assert!((norm - 1.0).abs() < 1e-5);
        }
        // Order is preserved: the fake's first component grows with the text length.
        assert!(vectors[0][0] < vectors[4][0]);
        let batches = seen.lock().unwrap().clone();
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
        assert_eq!(batches[0][0], "a", "no task prefix for other models");

        // The server switching to another vector length is detected.
        dims.store(4, Ordering::SeqCst);
        assert!(embedder.embed(&texts[..1], EmbedKind::Query).await.is_err());
    }

    #[tokio::test]
    async fn nomic_models_get_task_prefixes() {
        let (url, seen) = fake(Arc::new(AtomicUsize::new(3))).await;
        let embedder = OllamaEmbedder::new(&url, DEFAULT_OLLAMA_EMBED_MODEL, 8).unwrap();
        let text = vec!["where is the retry logic".to_owned()];
        embedder.embed(&text, EmbedKind::Query).await.unwrap();
        embedder.embed(&text, EmbedKind::Document).await.unwrap();
        let batches = seen.lock().unwrap().clone();
        assert_eq!(batches[0][0], "search_query: where is the retry logic");
        assert_eq!(batches[1][0], "search_document: where is the retry logic");
    }

    #[tokio::test]
    async fn server_errors_surface_as_errors() {
        let app = Router::new().route(
            "/api/embed",
            post(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(json!({ "error": "model \"nope\" not found" })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let embedder = OllamaEmbedder::new(&url, "nope", 8).unwrap();
        let err = embedder
            .embed(&["x".to_owned()], EmbedKind::Document)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert_eq!(embedder.dimensions(), 0);
    }
}
