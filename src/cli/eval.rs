//! `anchor eval`: scores the engine against a question set with known answers
//! (`ground_truth.json`, as written by `anchor demo`).
//!
//! Run it once with the models disabled and once with them enabled to see what they change:
//! intent classification accuracy, context recall, ownership accuracy, answer coverage, latency
//! and tokens spent.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;

use crate::base::{ClassifierKind, IntentKind, Result};
use crate::engine::query::{ItemRole, QueryEngine, QueryMode, QueryRequest};
use crate::metrics::TokenLedger;
use crate::synth::GroundTruth;

/// Scores of one category of questions.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CategoryScore {
    /// Questions asked.
    pub questions: usize,
    /// Questions whose intent was classified as expected.
    pub intent_correct: usize,
    /// Expected node keys found in the retrieved context / expected node keys.
    pub recall: f64,
    /// Questions whose top answer is an expected key (ownership: an expected owner).
    pub top_answer_correct: usize,
    /// Questions that produced an answer text.
    pub answered: usize,
    /// Answers containing every expected key phrase (justification questions only).
    pub phrases_matched: usize,
    /// Mean latency in milliseconds.
    pub mean_latency_ms: f64,
}

/// The result of an evaluation run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EvalReport {
    /// Scores per category (`ownership`, `justification`, `relationships`).
    pub categories: BTreeMap<String, CategoryScore>,
    /// Questions classified by the System 1 model rather than by heuristics.
    pub classified_by_model: usize,
    /// Answers synthesised by a model, per tier name.
    pub answered_by_tier: BTreeMap<String, usize>,
    /// Model calls made during the run.
    pub model_calls: u64,
    /// Prompt plus completion tokens spent during the run.
    pub tokens: u64,
    /// Wall-clock seconds.
    pub seconds: f64,
}

struct Question<'a> {
    text: &'a str,
    intents: &'a [IntentKind],
    expected: &'a [String],
    phrases: &'a [String],
}

/// Runs every question of `truth` through `queries`.
pub async fn evaluate(
    queries: &QueryEngine,
    ledger: &TokenLedger,
    truth: &GroundTruth,
    mode: QueryMode,
) -> Result<EvalReport> {
    let started = Instant::now();
    ledger.flush()?;
    let before = ledger.totals()?.total;
    let mut report = EvalReport::default();
    let no_phrases: Vec<String> = Vec::new();

    let groups: Vec<(&str, Vec<Question<'_>>)> = vec![
        (
            "ownership",
            truth
                .ownership
                .iter()
                .map(|q| Question {
                    text: &q.question,
                    intents: &[IntentKind::Ownership],
                    expected: &q.expected_owner_keys,
                    phrases: &no_phrases,
                })
                .collect(),
        ),
        (
            "justification",
            truth
                .justification
                .iter()
                .map(|q| Question {
                    text: &q.question,
                    intents: &[IntentKind::Justification, IntentKind::Status],
                    expected: &q.expected_keys,
                    phrases: &q.key_phrases,
                })
                .collect(),
        ),
        (
            "relationships",
            truth
                .relationships
                .iter()
                .map(|q| Question {
                    text: &q.question,
                    intents: &[
                        IntentKind::Ownership,
                        IntentKind::Dependency,
                        IntentKind::Definition,
                    ],
                    expected: &q.expected_keys,
                    phrases: &no_phrases,
                })
                .collect(),
        ),
    ];

    for (name, questions) in groups {
        let mut score = CategoryScore {
            questions: questions.len(),
            ..CategoryScore::default()
        };
        let (mut found, mut expected, mut latency) = (0usize, 0usize, 0f64);
        for question in &questions {
            let mut request = QueryRequest::new(question.text);
            request.mode = mode;
            let asked = Instant::now();
            let response = queries.query(request).await?;
            latency += asked.elapsed().as_secs_f64() * 1e3;
            if question.intents.contains(&response.intent.kind) {
                score.intent_correct += 1;
            }
            if response.intent.classifier == ClassifierKind::Model {
                report.classified_by_model += 1;
            }
            if let Some(tier) = response.route.tier {
                *report
                    .answered_by_tier
                    .entry(tier.as_str().to_owned())
                    .or_default() += 1;
            }
            expected += question.expected.len();
            found += question
                .expected
                .iter()
                .filter(|key| {
                    response
                        .context
                        .items
                        .iter()
                        .any(|item| &&item.node.key == key)
                })
                .count();
            let top = response
                .context
                .items
                .iter()
                .find(|item| item.role == ItemRole::Answer);
            if top.is_some_and(|item| question.expected.contains(&item.node.key)) {
                score.top_answer_correct += 1;
            }
            if let Some(answer) = &response.answer {
                score.answered += 1;
                let lower = answer.to_lowercase();
                if !question.phrases.is_empty()
                    && question
                        .phrases
                        .iter()
                        .all(|phrase| lower.contains(&phrase.to_lowercase()))
                {
                    score.phrases_matched += 1;
                }
            }
        }
        score.recall = found as f64 / expected.max(1) as f64;
        score.mean_latency_ms = latency / questions.len().max(1) as f64;
        report.categories.insert(name.to_owned(), score);
    }

    ledger.flush()?;
    let after = ledger.totals()?.total;
    report.model_calls = after.calls - before.calls;
    report.tokens = (after.prompt_tokens + after.completion_tokens)
        - (before.prompt_tokens + before.completion_tokens);
    report.seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

/// Renders the report as an aligned table.
pub fn render(report: &EvalReport) -> String {
    let mut out = format!(
        "{:<15} {:>4}  {:>7}  {:>7}  {:>8}  {:>9}  {:>8}  {:>8}\n",
        "category", "n", "intent", "recall", "top-ok", "answered", "phrases", "ms/query"
    );
    for (name, score) in &report.categories {
        let n = score.questions.max(1) as f64;
        out.push_str(&format!(
            "{:<15} {:>4}  {:>7.2}  {:>7.2}  {:>8.2}  {:>9.2}  {:>8}  {:>8.1}\n",
            name,
            score.questions,
            score.intent_correct as f64 / n,
            score.recall,
            score.top_answer_correct as f64 / n,
            score.answered as f64 / n,
            if name == "justification" {
                format!("{:.2}", score.phrases_matched as f64 / n)
            } else {
                "-".to_owned()
            },
            score.mean_latency_ms
        ));
    }
    out.push_str(&format!(
        "classified by model: {}   answered by tier: {:?}   model calls: {}   tokens: {}   total: {:.1} s\n",
        report.classified_by_model, report.answered_by_tier, report.model_calls, report.tokens, report.seconds
    ));
    out
}
