//! Initial graph filtering (System 1): scoring candidate nodes for relevance to a query.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::intent::extract_json;
use crate::base::{CallPurpose, CompletionRequest, NodeKind, ResponseFormat};

/// Most candidates judged by the model in one prompt; the rest are scored lexically.
pub const MAX_FILTER_CANDIDATES: usize = 48;
/// Longest snippet (in characters) shown to the model per candidate.
const MAX_SNIPPET_CHARS: usize = 240;

/// A node that might be relevant to a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterCandidate {
    /// Caller-chosen identifier, echoed back in [`ScoredCandidate::id`].
    pub id: String,
    /// Node kind.
    pub kind: NodeKind,
    /// Display name.
    pub name: String,
    /// A short excerpt of the node's text; may be empty.
    pub snippet: String,
}

/// Relevance of one candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredCandidate {
    /// The candidate's identifier.
    pub id: String,
    /// Relevance in `[0, 1]`.
    pub score: f32,
}

const STOPWORDS: &[&str] = &[
    "the", "an", "is", "are", "was", "of", "to", "in", "on", "at", "by", "for", "and", "or", "it",
    "this", "that", "who", "what", "why", "how", "where", "which", "does", "do", "with", "be",
    "as", "if", "we", "our",
];

fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= 2 && !STOPWORDS.contains(token))
        .map(str::to_owned)
        .collect()
}

/// Deterministic relevance: the share of query terms found in the candidate, a term in the name
/// counting fully and one found only in the snippet counting half. One score per candidate, in
/// input order.
pub fn lexical_scores(query: &str, candidates: &[FilterCandidate]) -> Vec<ScoredCandidate> {
    let query_tokens = tokens(query);
    candidates
        .iter()
        .map(|candidate| {
            let name = tokens(&candidate.name);
            let snippet = tokens(&candidate.snippet);
            let hits: f32 = query_tokens
                .iter()
                .map(|token| {
                    if name.contains(token) {
                        1.0
                    } else if snippet.contains(token) {
                        0.5
                    } else {
                        0.0
                    }
                })
                .sum();
            ScoredCandidate {
                id: candidate.id.clone(),
                score: if query_tokens.is_empty() {
                    0.0
                } else {
                    (hits / query_tokens.len() as f32).clamp(0.0, 1.0)
                },
            }
        })
        .collect()
}

/// Builds the batched scoring request for the first [`MAX_FILTER_CANDIDATES`] candidates.
/// Candidates are referred to by position so that ids never have to survive a round trip
/// through the model.
pub fn filter_request(
    query: &str,
    candidates: &[FilterCandidate],
    query_id: Option<&str>,
) -> CompletionRequest {
    let mut prompt = format!("Question: {}\n\nCandidates:\n", query.trim());
    for (index, candidate) in candidates.iter().take(MAX_FILTER_CANDIDATES).enumerate() {
        let snippet: String = candidate
            .snippet
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_SNIPPET_CHARS)
            .collect();
        prompt.push_str(&format!(
            "[{index}] ({}) {}{}{snippet}\n",
            candidate.kind,
            candidate.name,
            if snippet.is_empty() { "" } else { ": " },
        ));
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "scores": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"index": {"type": "integer"}, "score": {"type": "number"}},
                    "required": ["index", "score"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["scores"],
        "additionalProperties": false
    });
    let shown = candidates.len().min(MAX_FILTER_CANDIDATES);
    let mut request = CompletionRequest::prompt(CallPurpose::Filter, prompt)
        .with_system(
            "You judge which graph nodes help answer a question about a software organisation. \
             Reply with JSON only: {\"scores\":[{\"index\":0,\"score\":0.0}, ...]} with one entry \
             per candidate; score is 0 (irrelevant) to 1 (directly answers or names the subject).",
        )
        // About a dozen tokens per entry, plus slack.
        .with_max_tokens(u32::try_from(shown * 16 + 64).unwrap_or(u32::MAX))
        .with_temperature(0.0)
        .with_format(ResponseFormat::JsonSchema(schema));
    request.query_id = query_id.map(str::to_owned);
    request
}

fn as_score(value: &Value) -> Option<f32> {
    let score = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    score.is_finite().then(|| score.clamp(0.0, 1.0) as f32)
}

fn as_index(value: &Value) -> Option<usize> {
    match value {
        Value::Number(number) => usize::try_from(number.as_u64()?).ok(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Reads the model's answer to a [`filter_request`] over `count` candidates: `Some(score)` at each
/// position the model scored validly, `None` elsewhere. Returns `None` when nothing usable was
/// found. Accepts `{"scores":[{"index","score"}]}`, a bare array of such entries, a positional
/// array of numbers, and an `{"<index>": score}` map, inside prose or fences.
pub fn parse_filter_scores(output: &str, count: usize) -> Option<Vec<Option<f32>>> {
    let mut scores: Vec<Option<f32>> = vec![None; count];
    let value = extract_json(output, '{').or_else(|| extract_json(output, '['))?;
    let entries = match &value {
        Value::Object(map) => match map.get("scores").or_else(|| map.get("results")) {
            Some(inner) => inner.clone(),
            None => value.clone(),
        },
        _ => value.clone(),
    };
    match &entries {
        Value::Array(items) => {
            for (position, item) in items.iter().enumerate() {
                let (index, score) = match item {
                    Value::Object(entry) => (
                        entry
                            .get("index")
                            .or_else(|| entry.get("id"))
                            .and_then(as_index),
                        entry
                            .get("score")
                            .or_else(|| entry.get("relevance"))
                            .and_then(as_score),
                    ),
                    other => (Some(position), as_score(other)),
                };
                if let (Some(index), Some(score)) = (index, score)
                    && let Some(slot) = scores.get_mut(index)
                {
                    *slot = Some(score);
                }
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                if let (Ok(index), Some(score)) = (key.trim().parse::<usize>(), as_score(item))
                    && let Some(slot) = scores.get_mut(index)
                {
                    *slot = Some(score);
                }
            }
        }
        _ => {}
    }
    scores.iter().any(Option::is_some).then_some(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str, snippet: &str) -> FilterCandidate {
        FilterCandidate {
            id: id.into(),
            kind: NodeKind::Service,
            name: name.into(),
            snippet: snippet.into(),
        }
    }

    #[test]
    fn lexical_scores_rank_by_overlap() {
        let candidates = [
            candidate("a", "payments-api", "Charges cards"),
            candidate("b", "ledger", "Stores payments made through the api"),
            candidate("c", "search", ""),
        ];
        let scores = lexical_scores("Who owns the payments API?", &candidates);
        assert_eq!(
            scores.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Query terms: owns, payments, api.
        assert!((scores[0].score - 2.0 / 3.0).abs() < 1e-6);
        assert!((scores[1].score - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(scores[2].score, 0.0);
        assert_eq!(lexical_scores("?", &candidates)[0].score, 0.0);
    }

    #[test]
    fn request_lists_candidates_by_index() {
        let candidates: Vec<_> = (0..60)
            .map(|i| candidate(&format!("id{i}"), &format!("svc{i}"), "multi\n line   text"))
            .collect();
        let request = filter_request("q?", &candidates, Some("q-9"));
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("[0] (service) svc0: multi line text"));
        assert!(prompt.contains("[47] "));
        assert!(!prompt.contains("[48] "));
        assert_eq!(request.purpose, CallPurpose::Filter);
        assert_eq!(request.query_id.as_deref(), Some("q-9"));
    }

    #[test]
    fn scores_are_parsed_leniently() {
        let canonical = r#"{"scores":[{"index":1,"score":0.9},{"index":0,"score":"0.25"},{"index":7,"score":1},{"index":2,"score":"high"}]}"#;
        assert_eq!(
            parse_filter_scores(canonical, 3),
            Some(vec![Some(0.25), Some(0.9), None])
        );
        assert_eq!(
            parse_filter_scores("```json\n[0.5, 3, -1]\n```", 3),
            Some(vec![Some(0.5), Some(1.0), Some(0.0)])
        );
        assert_eq!(
            parse_filter_scores(r#"Scores: {"0": 0.1, "2": 0.7}"#, 3),
            Some(vec![Some(0.1), None, Some(0.7)])
        );
        assert_eq!(parse_filter_scores("no idea", 3), None);
        assert_eq!(parse_filter_scores(r#"{"scores": []}"#, 3), None);
    }
}
