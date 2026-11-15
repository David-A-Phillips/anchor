//! In-process sentence embeddings with Candle (cargo feature `embed-candle`).
//!
//! Runs a BERT-family encoder (default `sentence-transformers/all-MiniLM-L6-v2`) on the CPU:
//! tokenise with truncation and batch padding, run the encoder, mean-pool the token states under
//! the attention mask and L2-normalise, which is exactly the sentence-transformers recipe.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use super::l2_normalize;
use crate::base::{EmbedKind, Embedder, Error, Result};

/// Texts are truncated to this many tokens (the limit the default model was trained with).
pub const MAX_TOKENS: usize = 256;

/// A sentence-embedding model loaded from a local directory.
pub struct CandleEmbedder {
    inner: Arc<Inner>,
    model_id: String,
    batch_size: usize,
}

struct Inner {
    model: BertModel,
    tokenizer: Tokenizer,
    dimensions: usize,
}

impl std::fmt::Debug for CandleEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleEmbedder")
            .field("model_id", &self.model_id)
            .field("dimensions", &self.inner.dimensions)
            .field("batch_size", &self.batch_size)
            .finish()
    }
}

fn model_error(context: &str, err: impl std::fmt::Display) -> Error {
    Error::other(format!("embedding model: {context}: {err}"))
}

impl CandleEmbedder {
    /// Loads `config.json`, `tokenizer.json` and `model.safetensors` from `dir`.
    ///
    /// `repo_id` only names the model (the model id becomes `candle:<repo_id>`); `batch_size` is
    /// the number of texts per forward pass. The weights are read into memory with ordinary file
    /// I/O (no memory mapping). Blocking: call it off the async executor.
    pub fn load(dir: &Path, repo_id: &str, batch_size: usize) -> Result<Self> {
        let read = |file: &str| {
            let path = dir.join(file);
            std::fs::read(&path).map_err(|err| {
                Error::config(format!(
                    "cannot read embedding model file {}: {err}",
                    path.display()
                ))
            })
        };
        let config: Config = serde_json::from_slice(&read("config.json")?)
            .map_err(|err| model_error("config.json is not a BERT configuration", err))?;
        let mut tokenizer = Tokenizer::from_bytes(read("tokenizer.json")?)
            .map_err(|err| model_error("tokenizer.json is invalid", err))?;

        let max_length = MAX_TOKENS.min(config.max_position_embeddings);
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length,
                ..TruncationParams::default()
            }))
            .map_err(|err| model_error("cannot configure truncation", err))?;
        // Keep the tokenizer's own padding token when it declares one; pad to the longest text of
        // each batch rather than to a fixed length.
        let mut padding = tokenizer
            .get_padding()
            .cloned()
            .unwrap_or_else(|| PaddingParams {
                pad_id: u32::try_from(config.pad_token_id).unwrap_or(0),
                ..PaddingParams::default()
            });
        padding.strategy = PaddingStrategy::BatchLongest;
        padding.pad_to_multiple_of = None;
        tokenizer.with_padding(Some(padding));

        let weights = read("model.safetensors")?;
        let builder = VarBuilder::from_buffered_safetensors(weights, DType::F32, &Device::Cpu)
            .map_err(|err| model_error("model.safetensors is invalid", err))?;
        let model = BertModel::load(builder, &config)
            .map_err(|err| model_error("weights do not match config.json", err))?;

        Ok(Self {
            inner: Arc::new(Inner {
                model,
                tokenizer,
                dimensions: config.hidden_size,
            }),
            model_id: format!("candle:{repo_id}"),
            batch_size: batch_size.max(1),
        })
    }
}

impl Inner {
    fn embed(&self, texts: &[String], batch_size: usize) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(batch_size) {
            vectors.extend(self.embed_batch(batch)?);
        }
        Ok(vectors)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let encodings = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(|err| model_error("tokenisation failed", err))?;
        let rows = encodings.len();
        let length = encodings.first().map_or(0, |encoding| encoding.len());
        let hidden = self.dimensions;
        if length == 0 {
            // Only possible when every text is empty and the tokenizer adds no special tokens.
            return Ok(vec![vec![0.0; hidden]; rows]);
        }

        let mut ids = Vec::with_capacity(rows * length);
        let mut mask = Vec::with_capacity(rows * length);
        for encoding in &encodings {
            if encoding.len() != length {
                return Err(model_error(
                    "tokenisation failed",
                    "padding produced rows of different lengths",
                ));
            }
            ids.extend_from_slice(encoding.get_ids());
            mask.extend_from_slice(encoding.get_attention_mask());
        }

        let tensor = |err| model_error("inference failed", err);
        let device = Device::Cpu;
        let input_ids = Tensor::from_vec(ids, (rows, length), &device).map_err(tensor)?;
        let type_ids = input_ids.zeros_like().map_err(tensor)?;
        let attention = Tensor::from_vec(mask.clone(), (rows, length), &device).map_err(tensor)?;
        let states: Vec<f32> = self
            .model
            .forward(&input_ids, &type_ids, Some(&attention))
            .and_then(|output| output.to_dtype(DType::F32))
            .and_then(|output| output.flatten_all())
            .and_then(|output| output.to_vec1())
            .map_err(tensor)?;
        if states.len() != rows * length * hidden {
            return Err(model_error(
                "inference failed",
                "the model output has an unexpected shape",
            ));
        }

        // Mean pooling over real tokens only: padding must not influence a text's vector.
        let mut vectors = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut pooled = vec![0.0_f32; hidden];
            let mut tokens = 0.0_f32;
            for position in 0..length {
                if mask[row * length + position] == 0 {
                    continue;
                }
                tokens += 1.0;
                let start = (row * length + position) * hidden;
                for (sum, state) in pooled.iter_mut().zip(&states[start..start + hidden]) {
                    *sum += state;
                }
            }
            if tokens > 0.0 {
                for value in &mut pooled {
                    *value /= tokens;
                }
            }
            if pooled.iter().any(|value| !value.is_finite()) {
                return Err(model_error(
                    "inference failed",
                    "the model produced non-finite values",
                ));
            }
            l2_normalize(&mut pooled);
            vectors.push(pooled);
        }
        Ok(vectors)
    }
}

#[async_trait]
impl Embedder for CandleEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions
    }

    async fn embed(&self, texts: &[String], _kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self.inner.clone();
        let texts = texts.to_vec();
        let batch_size = self.batch_size;
        tokio::task::spawn_blocking(move || inner.embed(&texts, batch_size)).await?
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use candle_nn::VarMap;
    use serde_json::json;

    use super::*;
    use crate::engine::embed::{DownloadProgress, ensure_model};

    const HIDDEN: usize = 32;
    const WORDS: [&str; 12] = [
        "[PAD]", "[UNK]", "[CLS]", "[SEP]", "the", "payment", "service", "refunds", "orders",
        "graph", "index", "search",
    ];

    /// Writes a two-layer BERT with random weights and a word-level tokenizer into `dir`.
    fn tiny_model(dir: &Path) {
        let config = json!({
            "vocab_size": WORDS.len(), "hidden_size": HIDDEN, "num_hidden_layers": 2,
            "num_attention_heads": 2, "intermediate_size": 64, "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0, "max_position_embeddings": 64, "type_vocab_size": 2,
            "initializer_range": 0.02, "layer_norm_eps": 1e-12, "pad_token_id": 0,
            "position_embedding_type": "absolute", "use_cache": true,
            "classifier_dropout": null, "model_type": "bert"
        });
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();

        let vocab: serde_json::Map<String, serde_json::Value> = WORDS
            .iter()
            .enumerate()
            .map(|(id, word)| ((*word).to_owned(), json!(id)))
            .collect();
        let tokenizer = json!({
            "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
            "normalizer": { "type": "Lowercase" },
            "pre_tokenizer": { "type": "Whitespace" },
            "post_processor": {
                "type": "TemplateProcessing",
                "single": [
                    { "SpecialToken": { "id": "[CLS]", "type_id": 0 } },
                    { "Sequence": { "id": "A", "type_id": 0 } },
                    { "SpecialToken": { "id": "[SEP]", "type_id": 0 } }
                ],
                "pair": [
                    { "Sequence": { "id": "A", "type_id": 0 } },
                    { "Sequence": { "id": "B", "type_id": 1 } }
                ],
                "special_tokens": {
                    "[CLS]": { "id": "[CLS]", "ids": [2], "tokens": ["[CLS]"] },
                    "[SEP]": { "id": "[SEP]", "ids": [3], "tokens": ["[SEP]"] }
                }
            },
            "decoder": null,
            "model": { "type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]" }
        });
        std::fs::write(dir.join("tokenizer.json"), tokenizer.to_string()).unwrap();

        // Loading the model through a VarMap creates every variable with its default (random)
        // initialisation; saving the map yields a genuine safetensors checkpoint.
        let parsed: Config = serde_json::from_value(config).unwrap();
        let varmap = VarMap::new();
        let builder = VarBuilder::from_varmap(&varmap, DType::F32, &Device::Cpu);
        BertModel::load(builder, &parsed).unwrap();
        varmap.save(dir.join("model.safetensors")).unwrap();
    }

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn max_difference(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    #[tokio::test]
    async fn tiny_bert_embeds_with_correct_shape_norm_and_padding_invariance() {
        let dir = tempfile::tempdir().unwrap();
        tiny_model(dir.path());
        let embedder = CandleEmbedder::load(dir.path(), "test/tiny", 2).unwrap();
        assert_eq!(embedder.model_id(), "candle:test/tiny");
        assert_eq!(embedder.dimensions(), HIDDEN);

        let short = "payment service";
        let long = "the payment service refunds orders the graph index search the orders";
        // Batch size 2 over three texts: two forward passes, the first one padded.
        let batch = embedder
            .embed(&texts(&[short, long, "graph search"]), EmbedKind::Document)
            .await
            .unwrap();
        assert_eq!(batch.len(), 3);
        for vector in &batch {
            assert_eq!(vector.len(), HIDDEN);
            let norm: f32 = vector.iter().map(|v| v * v).sum();
            assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
        }
        assert!(
            max_difference(&batch[0], &batch[1]) > 1e-3,
            "texts must differ"
        );

        // Deterministic, and independent of what else is in the batch (padding is masked out).
        let again = embedder
            .embed(&texts(&[short, long, "graph search"]), EmbedKind::Query)
            .await
            .unwrap();
        assert_eq!(batch, again);
        let alone = embedder
            .embed(&texts(&[short]), EmbedKind::Document)
            .await
            .unwrap();
        assert!(max_difference(&alone[0], &batch[0]) < 1e-4);

        // Truncation: text far beyond the position table still embeds.
        let endless = vec!["orders"; 500].join(" ");
        let truncated = embedder
            .embed(&[endless], EmbedKind::Document)
            .await
            .unwrap();
        assert_eq!(truncated[0].len(), HIDDEN);
        assert!(
            embedder
                .embed(&[], EmbedKind::Document)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn broken_model_directories_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            CandleEmbedder::load(dir.path(), "test/tiny", 2),
            Err(Error::Config(_))
        ));
        tiny_model(dir.path());
        std::fs::write(dir.path().join("model.safetensors"), b"not a checkpoint").unwrap();
        assert!(CandleEmbedder::load(dir.path(), "test/tiny", 2).is_err());
    }

    /// Downloads the real default model (about 90 MB) and checks that it behaves semantically.
    /// Opt-in: `ANCHOR_TEST_HF_DOWNLOAD=1 cargo test -- --ignored real_model`.
    #[tokio::test]
    #[ignore = "downloads a model from Hugging Face; set ANCHOR_TEST_HF_DOWNLOAD=1"]
    async fn real_model_is_semantically_sane() {
        if std::env::var("ANCHOR_TEST_HF_DOWNLOAD").as_deref() != Ok("1") {
            eprintln!("skipped: ANCHOR_TEST_HF_DOWNLOAD is not 1");
            return;
        }
        let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp/hf-models");
        let repo = crate::engine::embed::DEFAULT_CANDLE_MODEL;
        let started = Instant::now();
        let dir = ensure_model(&models, repo, &|_: DownloadProgress| {})
            .await
            .unwrap();
        println!("ensure_model: {:?}", started.elapsed());

        let started = Instant::now();
        let embedder = CandleEmbedder::load(&dir, repo, 32).unwrap();
        println!("load: {:?}", started.elapsed());
        assert_eq!(embedder.dimensions(), 384);

        let sentences = texts(&[
            "How do I reset my password?",
            "I forgot my password and need to change it.",
            "The cargo ship left the harbour at dawn.",
        ]);
        let started = Instant::now();
        let vectors = embedder
            .embed(&sentences, EmbedKind::Document)
            .await
            .unwrap();
        println!("embed 3 sentences: {:?}", started.elapsed());
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let paraphrase = dot(&vectors[0], &vectors[1]);
        let unrelated = dot(&vectors[0], &vectors[2]);
        println!("paraphrase {paraphrase:.3}, unrelated {unrelated:.3}");
        assert!(paraphrase > 0.6, "paraphrase similarity {paraphrase}");
        assert!(paraphrase > unrelated + 0.3);

        let chunks: Vec<String> = (0..64)
            .map(|i| format!("fn handler_{i}(request: Request) -> Response {{ route(request) }}"))
            .collect();
        let started = Instant::now();
        embedder.embed(&chunks, EmbedKind::Document).await.unwrap();
        println!("embed 64 code chunks: {:?}", started.elapsed());
    }
}
