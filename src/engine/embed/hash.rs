//! Deterministic feature-hashing embedder.
//!
//! No model, no download, no network: text is tokenised with awareness of identifiers, every token,
//! token pair and character trigram is hashed into one of `dimensions` buckets with a pseudo-random
//! sign (the "hashing trick"), and the result is L2-normalised. Texts sharing vocabulary end up
//! close; it is a lexical embedding, not a semantic one.
//!
//! Everything that influences the output is pinned: a hand-written FNV-1a hash, features summed in
//! sorted order, and only IEEE-exact float operations (`sqrt`, `+`, `*`, `/`), so vectors are
//! bit-identical across platforms, runs and compiler versions. Any change to the scheme must bump
//! the version in the model id.

use async_trait::async_trait;

use super::index::MAX_DIMENSIONS;
use super::l2_normalize;
use crate::base::{EmbedKind, Embedder, Error, Result};

/// Batches above this many bytes of text are hashed on the blocking pool.
const INLINE_BYTES: usize = 64 * 1024;

const KIND_UNIGRAM: u8 = 1;
const KIND_BIGRAM: u8 = 2;
const KIND_TRIGRAM: u8 = 3;

/// Relative weight of each feature family. Whole words dominate, word pairs add phrase
/// sensitivity, character trigrams make inflections and typos land near each other.
fn kind_weight(kind: u8) -> f32 {
    match kind {
        KIND_UNIGRAM => 1.0,
        KIND_BIGRAM => 0.6,
        _ => 0.25,
    }
}

/// The zero-download default embedder. See the module documentation.
#[derive(Clone, Debug)]
pub struct HashEmbedder {
    dimensions: usize,
    model_id: String,
}

impl HashEmbedder {
    /// An embedder producing vectors of `dimensions` floats (8 to [`MAX_DIMENSIONS`]).
    pub fn new(dimensions: usize) -> Result<Self> {
        if !(8..=MAX_DIMENSIONS).contains(&dimensions) {
            return Err(Error::config(format!(
                "embeddings.dimensions must be between 8 and {MAX_DIMENSIONS}, got {dimensions}"
            )));
        }
        Ok(Self {
            dimensions,
            model_id: format!("hash-v1-{dimensions}"),
        })
    }

    /// Embeds one text synchronously. Text without any letter or digit yields the zero vector.
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let tokens = tokenize(text);
        let mut features: Vec<(u64, u8)> = Vec::with_capacity(tokens.len() * 8);
        for (position, token) in tokens.iter().enumerate() {
            features.push((
                feature_hash(KIND_UNIGRAM, &[token.as_bytes()]),
                KIND_UNIGRAM,
            ));
            if let Some(next) = tokens.get(position + 1) {
                features.push((
                    feature_hash(KIND_BIGRAM, &[token.as_bytes(), next.as_bytes()]),
                    KIND_BIGRAM,
                ));
            }
            push_trigrams(token, &mut features);
        }
        // Sorting groups equal features (for term frequency) and fixes the summation order, which
        // keeps the floating-point result reproducible.
        features.sort_unstable();

        let mut vector = vec![0.0_f32; self.dimensions];
        let mut start = 0;
        while start < features.len() {
            let (hash, kind) = features[start];
            let mut end = start + 1;
            while end < features.len() && features[end] == (hash, kind) {
                end += 1;
            }
            // Sub-linear term weighting: a word repeated nine times counts three times.
            let weight = kind_weight(kind) * ((end - start) as f32).sqrt();
            let bucket = (hash % self.dimensions as u64) as usize;
            if hash >> 63 == 0 {
                vector[bucket] += weight;
            } else {
                vector[bucket] -= weight;
            }
            start = end;
        }
        l2_normalize(&mut vector);
        vector
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, texts: &[String], _kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let bytes: usize = texts.iter().map(String::len).sum();
        if bytes <= INLINE_BYTES {
            return Ok(texts.iter().map(|text| self.embed_one(text)).collect());
        }
        let embedder = self.clone();
        let texts = texts.to_vec();
        Ok(tokio::task::spawn_blocking(move || {
            texts.iter().map(|text| embedder.embed_one(text)).collect()
        })
        .await?)
    }
}

/// Splits text into lowercase word tokens, breaking identifiers apart.
///
/// Anything that is not a letter or digit separates tokens (so snake_case, kebab-case, paths and
/// punctuation split), and inside a run a new token starts at a lower-to-upper transition
/// (`camelCase`), at the last capital of an acronym followed by a lowercase letter (`HTTPServer` ->
/// `http`, `server`) and between letters and digits. `PaymentsService` therefore tokenises exactly
/// like `payments service`.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_alphanumeric() {
            flush(&mut current, &mut tokens);
            previous = None;
            continue;
        }
        if let Some(p) = previous {
            let next_is_lower = chars.peek().is_some_and(|n| n.is_lowercase());
            let boundary = (p.is_lowercase() && c.is_uppercase())
                || (p.is_uppercase() && c.is_uppercase() && next_is_lower)
                || (p.is_numeric() != c.is_numeric());
            if boundary {
                flush(&mut current, &mut tokens);
            }
        }
        current.extend(c.to_lowercase());
        previous = Some(c);
    }
    flush(&mut current, &mut tokens);
    tokens
}

fn flush(current: &mut String, tokens: &mut Vec<String>) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

/// Character trigrams of `^token$`; tokens shorter than three characters contribute none (their
/// unigram already says everything).
fn push_trigrams(token: &str, features: &mut Vec<(u64, u8)>) {
    let mut chars: Vec<char> = Vec::with_capacity(token.len() + 2);
    chars.push('^');
    chars.extend(token.chars());
    chars.push('$');
    if chars.len() < 5 {
        return;
    }
    let mut buffer = [0_u8; 12];
    for window in chars.windows(3) {
        let mut len = 0;
        for c in window {
            len += c.encode_utf8(&mut buffer[len..]).len();
        }
        features.push((feature_hash(KIND_TRIGRAM, &[&buffer[..len]]), KIND_TRIGRAM));
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 64-bit FNV-1a, continuing from `state`.
fn fnv1a(mut state: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(FNV_PRIME);
    }
    state
}

/// Hash of one feature: its family, then its parts separated by a byte that cannot occur in UTF-8.
/// FNV's high bits are weak for short inputs, so a final avalanche step (the MurmurHash3
/// finaliser) makes both the bucket (low bits) and the sign (top bit) well distributed.
fn feature_hash(kind: u8, parts: &[&[u8]]) -> u64 {
    let mut state = fnv1a(FNV_OFFSET, &[kind]);
    for part in parts {
        state = fnv1a(state, part);
        state = fnv1a(state, &[0xff]);
    }
    state ^= state >> 33;
    state = state.wrapping_mul(0xff51_afd7_ed55_8ccd);
    state ^= state >> 33;
    state = state.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    state ^ (state >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn tokenisation_splits_identifiers() {
        assert_eq!(tokenize("PaymentsService"), ["payments", "service"]);
        assert_eq!(
            tokenize("parseHTTPResponse_v2 kebab-case"),
            ["parse", "http", "response", "v", "2", "kebab", "case"]
        );
        assert_eq!(
            tokenize("src/engine/embed/mod.rs: fn main()"),
            ["src", "engine", "embed", "mod", "rs", "fn", "main"]
        );
        assert!(tokenize(" -- !! ").is_empty());
    }

    #[test]
    fn fnv_matches_published_test_vectors() {
        assert_eq!(fnv1a(FNV_OFFSET, b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(FNV_OFFSET, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(FNV_OFFSET, b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn related_texts_score_higher_than_unrelated_ones() {
        let embedder = HashEmbedder::new(384).unwrap();
        let identifier = embedder.embed_one("PaymentsService.processRefund");
        let prose = embedder.embed_one("payments service process refund");
        let inflected = embedder.embed_one("the payment services processed a refund");
        let unrelated = embedder.embed_one("kubernetes ingress tls certificate rotation");
        assert!(dot(&identifier, &prose) > 0.95);
        assert!(dot(&identifier, &inflected) > dot(&identifier, &unrelated) + 0.2);
        assert!(dot(&identifier, &unrelated).abs() < 0.2);
    }

    #[test]
    fn vectors_are_unit_length_and_empty_text_is_zero() {
        let embedder = HashEmbedder::new(64).unwrap();
        let v = embedder.embed_one("retry the request with exponential backoff");
        assert_eq!(v.len(), 64);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-5);
        assert!(embedder.embed_one("...").iter().all(|x| *x == 0.0));
        assert!(HashEmbedder::new(0).is_err());
    }

    #[tokio::test]
    async fn outputs_are_bit_stable() {
        let embedder = HashEmbedder::new(384).unwrap();
        let text = "fn verify_signature(pack: &ContextPack) -> Result<DeviceId>".to_owned();
        let direct = embedder.embed_one(&text);
        let batch = embedder
            .embed(std::slice::from_ref(&text), EmbedKind::Document)
            .await
            .unwrap();
        assert_eq!(batch[0], direct);
        // Golden digest of the exact bit pattern: fails if the hash, the tokeniser, the weights
        // or the summation order change, which requires a new model id version.
        let mut bytes = Vec::new();
        for value in &direct {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        assert_eq!(
            blake3::hash(&bytes).to_hex().as_str(),
            "ded5b6366b1c675765a26ccb7f1a4043df5ec78c7aac6f5ddb8f3ffca671a627",
            "hash embedder output changed"
        );
    }

    #[tokio::test]
    async fn large_batches_run_on_the_blocking_pool() {
        let embedder = HashEmbedder::new(128).unwrap();
        let texts: Vec<String> = (0..40).map(|i| format!("chunk {i} ").repeat(400)).collect();
        let vectors = embedder.embed(&texts, EmbedKind::Document).await.unwrap();
        assert_eq!(vectors.len(), 40);
        assert_eq!(vectors[7], embedder.embed_one(&texts[7]));
    }
}
