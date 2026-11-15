//! `anchor compare`: scores Anchor next to other retrieval systems on the same questions.
//!
//! Everything goes through a system-neutral format so that any RAG system can take part:
//!
//! * `questions.jsonl`: one question per line, with the surface forms (names, aliases) of each
//!   expected answer and of each known-stale answer. No Anchor node keys are needed to score.
//! * `results/<system>.jsonl`: per question, the text contexts the system retrieved (best first),
//!   optionally the system's own answer, and optionally an answer generated from those contexts
//!   by a shared model (`generated_answer`; see `scripts/compare/generate.py`).
//!
//! The scorer reads only text, cuts every system's contexts to the same budget, and applies the
//! same string matching to all of them. Anchor's own results and three plain-retrieval baselines
//! over Anchor's chunks (full-text, vector, and both fused, with no graph and no trust) are
//! written by this command; other systems write their own results file.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::base::{ChunkId, EmbedKind, NodeKey, NodeKind, Result, keys};
use crate::engine::Engine;
use crate::engine::query::{ItemRole, QueryEngine, QueryMode, QueryRequest};
use crate::engine::store::GraphStore;
use crate::synth::GroundTruth;

/// Contexts considered per question. Systems choose how many to return and how large they are;
/// the token budget below is what actually bounds them.
pub const MAX_CONTEXTS: usize = 50;
/// Context budget per question, in approximate tokens (four characters each).
pub const CONTEXT_TOKENS: usize = 6000;
const CANDIDATES: usize = 50;
const RRF_K: f64 = 60.0;

/// One question in the neutral format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// Unique id.
    pub id: String,
    /// `ownership`, `justification` or `relationships`.
    pub category: String,
    /// Finer kind or scenario (`conflict`, `inherited`, `service_portfolio`, ...).
    pub kind: String,
    /// The question text.
    pub question: String,
    /// One group per expected answer; a group is found when any of its forms appears.
    pub expected: Vec<Vec<String>>,
    /// One group per known-stale answer that a less trusted source still asserts.
    #[serde(default)]
    pub stale: Vec<Vec<String>>,
    /// `true` when `expected` holds phrases to be quoted rather than entity names.
    #[serde(default)]
    pub phrases: bool,
}

/// What one system returned for one question.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemResult {
    /// Question id.
    pub id: String,
    /// Retrieved contexts, best first.
    #[serde(default)]
    pub contexts: Vec<String>,
    /// The system's own answer, if it produces one.
    #[serde(default)]
    pub answer: Option<String>,
    /// An answer written from `contexts` by the shared generator model.
    #[serde(default)]
    pub generated_answer: Option<String>,
    /// Retrieval latency in milliseconds.
    #[serde(default)]
    pub latency_ms: f64,
}

/// Lower-cases and reduces every run of non-alphanumeric characters to one space, padded with
/// spaces so that containment checks respect word boundaries.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    let mut gap = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            gap = false;
        } else if !gap {
            out.push(' ');
            gap = true;
        }
    }
    if !gap {
        out.push(' ');
    }
    out
}

/// Position of the first appearance of any form of `group` in normalised `text`.
fn find(text: &str, group: &[String]) -> Option<usize> {
    group
        .iter()
        .filter_map(|form| {
            let form = normalize(form);
            (form.len() > 2).then(|| text.find(&form)).flatten()
        })
        .min()
}

/// Like [`find`], but a phrase also counts when most of its longer words appear: generated
/// answers paraphrase.
fn find_loose(text: &str, group: &[String]) -> Option<usize> {
    if let Some(at) = find(text, group) {
        return Some(at);
    }
    group.iter().find_map(|form| {
        let form = normalize(form);
        let words: Vec<&str> = form.split(' ').filter(|w| w.len() > 3).collect();
        if words.len() < 3 {
            return None;
        }
        let present = words
            .iter()
            .filter(|word| text.contains(&format!(" {word} ")))
            .count();
        (present * 10 >= words.len() * 7).then_some(0)
    })
}

fn forms(store: &GraphStore, key: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(node) = store.node_by_key(&NodeKey::new(key))? {
        out.push(node.name.clone());
        out.extend(node.aliases.iter().cloned());
        for other in &node.keys {
            if let Some((_, slug)) = other.as_str().split_once(':') {
                out.push(slug.to_owned());
            }
        }
    } else if let Some((_, slug)) = key.split_once(':') {
        out.push(slug.to_owned());
    }
    out.sort();
    out.dedup_by_key(|form| normalize(form));
    Ok(out)
}

fn groups(store: &GraphStore, keys: &[String]) -> Result<Vec<Vec<String>>> {
    keys.iter().map(|key| forms(store, key)).collect()
}

/// Converts a ground truth into neutral questions, looking up the names of expected entities.
pub fn questions(store: &GraphStore, truth: &GroundTruth) -> Result<Vec<Question>> {
    let mut out = Vec::new();
    for q in &truth.ownership {
        let stale: Vec<String> = q
            .stale_claim_keys
            .iter()
            .filter(|key| {
                matches!(
                    keys::kind_of(&NodeKey::new(key.as_str())),
                    Some(NodeKind::Team | NodeKind::Person)
                )
            })
            .cloned()
            .collect();
        out.push(Question {
            id: q.id.clone(),
            category: "ownership".into(),
            kind: serde_json::to_value(q.scenario)?
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            question: q.question.clone(),
            expected: groups(store, &q.expected_owner_keys)?,
            stale: groups(store, &stale)?,
            phrases: false,
        });
    }
    for q in &truth.justification {
        out.push(Question {
            id: q.id.clone(),
            category: "justification".into(),
            kind: q.kind.as_str().to_owned(),
            question: q.question.clone(),
            expected: q.key_phrases.iter().map(|p| vec![p.clone()]).collect(),
            stale: Vec::new(),
            phrases: true,
        });
    }
    for q in &truth.relationships {
        out.push(Question {
            id: q.id.clone(),
            category: "relationships".into(),
            kind: q.kind.as_str().to_owned(),
            question: q.question.clone(),
            expected: groups(store, &q.expected_keys)?,
            stale: Vec::new(),
            phrases: false,
        });
    }
    out.retain(|q| !q.expected.is_empty() && q.expected.iter().all(|g| !g.is_empty()));
    Ok(out)
}

/// Runs the questions through Anchor's query engine as configured (by default: retrieval and
/// direct graph answers, no model).
pub async fn run_anchor(
    queries: &QueryEngine,
    questions: &[Question],
) -> Result<Vec<SystemResult>> {
    let mut out = Vec::with_capacity(questions.len());
    for question in questions {
        let mut request = QueryRequest::new(question.question.as_str());
        request.mode = QueryMode::Auto;
        let started = Instant::now();
        let response = queries.query(request).await?;
        let latency_ms = started.elapsed().as_secs_f64() * 1e3;
        // What an agent receives from the `ask` tool: the direct answer when the graph holds
        // one, resolved conflicts, then the evidence.
        let mut contexts: Vec<String> = response
            .answer
            .iter()
            .map(|answer| format!("Direct answer from the knowledge graph: {answer}"))
            .collect();
        contexts.extend(
            response
                .context
                .conflicts
                .iter()
                .map(|conflict| format!("Conflict resolved: {}", conflict.explanation)),
        );
        for item in &response.context.items {
            let role = match item.role {
                ItemRole::Answer => "answer",
                ItemRole::Evidence => "evidence",
                ItemRole::Overridden => "OVERRIDDEN by a more authoritative source; do not use",
            };
            let mut text = format!(
                "{} \"{}\" [{}; trust: {}]",
                item.node.kind, item.node.name, role, item.node.trust
            );
            let via = item.path_text();
            if !via.is_empty() {
                text.push_str(&format!("\nreached via: {via}"));
            }
            for snippet in &item.snippets {
                text.push_str(&format!(
                    "\n({}, {}) {}",
                    snippet.source, snippet.trust, snippet.text
                ));
            }
            contexts.push(text);
        }
        out.push(SystemResult {
            id: question.id.clone(),
            contexts,
            answer: response.answer,
            generated_answer: None,
            latency_ms,
        });
    }
    Ok(out)
}

/// A plain retrieval pipeline over Anchor's chunks: no graph, no trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    /// BM25 over chunk text.
    FullText,
    /// Nearest neighbours of the question's embedding.
    Vector,
    /// Reciprocal-rank fusion of both.
    Hybrid,
}

impl Baseline {
    /// Every baseline.
    pub const ALL: [Baseline; 3] = [Baseline::FullText, Baseline::Vector, Baseline::Hybrid];

    /// File and table name.
    pub const fn name(self) -> &'static str {
        match self {
            Baseline::FullText => "baseline-fulltext",
            Baseline::Vector => "baseline-vector",
            Baseline::Hybrid => "baseline-hybrid",
        }
    }
}

/// Runs the questions through a plain retrieval baseline over the engine's own chunks.
pub async fn run_baseline(
    engine: &Engine,
    baseline: Baseline,
    questions: &[Question],
) -> Result<Vec<SystemResult>> {
    let store = engine.store();
    let mut out = Vec::with_capacity(questions.len());
    for question in questions {
        let started = Instant::now();
        let lexical: Vec<ChunkId> = if baseline == Baseline::Vector {
            Vec::new()
        } else {
            store
                .search_chunks(&question.question, CANDIDATES)?
                .into_iter()
                .map(|hit| hit.chunk)
                .collect()
        };
        let dense: Vec<ChunkId> = if baseline == Baseline::FullText {
            Vec::new()
        } else {
            let vector = engine
                .embedder()
                .embed(std::slice::from_ref(&question.question), EmbedKind::Query)
                .await?
                .into_iter()
                .next()
                .unwrap_or_default();
            engine
                .vectors()
                .search_async(vector, CANDIDATES, None)
                .await?
                .into_iter()
                .map(|hit| hit.id)
                .collect()
        };
        let mut fused: HashMap<ChunkId, f64> = HashMap::new();
        for list in [&lexical, &dense] {
            for (rank, chunk) in list.iter().enumerate() {
                *fused.entry(*chunk).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }
        let mut ranked: Vec<(ChunkId, f64)> = fused.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut contexts = Vec::new();
        for (chunk, _) in ranked.into_iter().take(MAX_CONTEXTS) {
            let Some(chunk) = store.chunk(chunk)? else {
                continue;
            };
            let name = store.node(chunk.node)?.map(|n| n.name).unwrap_or_default();
            contexts.push(format!("[{}] {}\n{}", chunk.source, name, chunk.text));
        }
        out.push(SystemResult {
            id: question.id.clone(),
            contexts,
            answer: None,
            generated_answer: None,
            latency_ms: started.elapsed().as_secs_f64() * 1e3,
        });
    }
    Ok(out)
}

/// Scores of one system on one category.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Score {
    /// Questions scored.
    pub questions: usize,
    /// Mean share of expected answers present in the retrieved context.
    pub context_recall: f64,
    /// Share of questions with every expected answer in the context.
    pub context_complete: f64,
    /// Mean share of expected answers present in the system's own answer (0 when it gave none).
    pub answer_recall: Option<f64>,
    /// Questions for which the shared generator wrote an answer (a sample of all questions).
    pub generated_questions: usize,
    /// Mean share of expected answers present in the shared generator's answer.
    pub generated_recall: Option<f64>,
    /// Questions that have a stale claim.
    pub stale_questions: usize,
    /// Of those: the stale answer is absent from the context, or appears only after a correct one.
    pub stale_context_safe: f64,
    /// Questions with a stale claim that have a generated answer.
    pub stale_generated_questions: usize,
    /// Of those: the generated answer names a correct owner and does not lead with the stale one.
    pub stale_generated_safe: Option<f64>,
    /// Mean context size in approximate tokens, after the budget cut.
    pub context_tokens: f64,
    /// Mean retrieval latency in milliseconds.
    pub latency_ms: f64,
}

fn budgeted(contexts: &[String]) -> (String, usize) {
    let budget = CONTEXT_TOKENS * 4;
    let mut text = String::new();
    for context in contexts.iter().take(MAX_CONTEXTS) {
        if text.is_empty() && context.len() > budget {
            // One oversized context (some systems return a single blob): keep its head.
            let mut cut = budget;
            while !context.is_char_boundary(cut) {
                cut -= 1;
            }
            text.push_str(&context[..cut]);
            break;
        }
        if text.len() + context.len() > budget {
            break;
        }
        text.push_str(context);
        text.push_str("\n\n");
    }
    let tokens = text.len() / 4;
    (normalize(&text), tokens)
}

fn share(question: &Question, text: &str, loose: bool) -> f64 {
    let found = question
        .expected
        .iter()
        .filter(|group| {
            if loose && question.phrases {
                find_loose(text, group).is_some()
            } else {
                find(text, group).is_some()
            }
        })
        .count();
    found as f64 / question.expected.len().max(1) as f64
}

/// `true` when the text does not lead with a stale answer: no stale form appears, or a correct
/// form appears first. With `need_correct`, a correct form must appear at all.
fn stale_safe(question: &Question, text: &str, need_correct: bool) -> bool {
    let correct = question.expected.iter().filter_map(|g| find(text, g)).min();
    let stale = question.stale.iter().filter_map(|g| find(text, g)).min();
    match (correct, stale) {
        (None, None) => !need_correct,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (Some(correct), Some(stale)) => correct < stale,
    }
}

/// Scores one system's results per category (plus `all`).
pub fn score(questions: &[Question], results: &[SystemResult]) -> BTreeMap<String, Score> {
    let by_id: HashMap<&str, &SystemResult> = results.iter().map(|r| (r.id.as_str(), r)).collect();
    let any_answer = results.iter().any(|r| r.answer.is_some());
    let empty = SystemResult::default();
    let mut sums: BTreeMap<String, (Score, f64, f64, f64, f64)> = BTreeMap::new();
    for question in questions {
        let result = by_id.get(question.id.as_str()).copied().unwrap_or(&empty);
        let (context, tokens) = budgeted(&result.contexts);
        let recall = share(question, &context, false);
        let answer = result.answer.as_deref().map(normalize).unwrap_or_default();
        let generated = result.generated_answer.as_deref().map(normalize);
        for name in [question.category.as_str(), "all"] {
            let (score, answer_sum, generated_sum, stale_context, stale_generated) =
                sums.entry(name.to_owned()).or_default();
            score.questions += 1;
            score.context_recall += recall;
            score.context_complete += f64::from(recall >= 1.0);
            score.context_tokens += tokens as f64;
            score.latency_ms += result.latency_ms;
            *answer_sum += share(question, &answer, true);
            if let Some(generated) = &generated {
                score.generated_questions += 1;
                *generated_sum += share(question, generated, true);
            }
            if !question.stale.is_empty() {
                score.stale_questions += 1;
                *stale_context += f64::from(stale_safe(question, &context, false));
                if let Some(generated) = &generated {
                    score.stale_generated_questions += 1;
                    *stale_generated += f64::from(stale_safe(question, generated, true));
                }
            }
        }
    }
    sums.into_iter()
        .map(
            |(name, (mut score, answer, generated, stale_context, stale_generated))| {
                let n = score.questions.max(1) as f64;
                let stale = score.stale_questions.max(1) as f64;
                score.context_recall /= n;
                score.context_complete /= n;
                score.context_tokens /= n;
                score.latency_ms /= n;
                score.answer_recall = any_answer.then_some(answer / n);
                score.generated_recall = (score.generated_questions > 0)
                    .then(|| generated / score.generated_questions as f64);
                score.stale_context_safe = stale_context / stale;
                score.stale_generated_safe = (score.stale_generated_questions > 0)
                    .then(|| stale_generated / score.stale_generated_questions as f64);
                (name, score)
            },
        )
        .collect()
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for line in std::fs::read_to_string(path)?.lines() {
        if !line.trim().is_empty() {
            out.push(serde_json::from_str(line)?);
        }
    }
    Ok(out)
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row)?);
        text.push('\n');
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// Writes `questions.jsonl` and the results of Anchor and its plain-retrieval baselines into
/// `dir`. Answers generated earlier for the same contexts are kept.
pub async fn export(
    engine: &Engine,
    queries: &QueryEngine,
    truth: &GroundTruth,
    dir: &Path,
) -> Result<()> {
    let results_dir = dir.join("results");
    std::fs::create_dir_all(&results_dir)?;
    let questions = questions(engine.store(), truth)?;
    write_jsonl(&dir.join("questions.jsonl"), &questions)?;
    let mut systems = vec![("anchor", run_anchor(queries, &questions).await?)];
    for baseline in Baseline::ALL {
        systems.push((
            baseline.name(),
            run_baseline(engine, baseline, &questions).await?,
        ));
    }
    for (name, mut results) in systems {
        let path = results_dir.join(format!("{name}.jsonl"));
        if path.exists() {
            let previous: Vec<SystemResult> = read_jsonl(&path)?;
            let previous: HashMap<&str, &SystemResult> =
                previous.iter().map(|r| (r.id.as_str(), r)).collect();
            for result in &mut results {
                if let Some(old) = previous.get(result.id.as_str())
                    && old.contexts == result.contexts
                {
                    result.generated_answer.clone_from(&old.generated_answer);
                }
            }
        }
        write_jsonl(&path, &results)?;
    }
    Ok(())
}

/// Scores every `results/*.jsonl` in `dir` against `questions.jsonl`. Anchor comes first.
pub fn score_dir(dir: &Path) -> Result<Vec<(String, BTreeMap<String, Score>)>> {
    let questions: Vec<Question> = read_jsonl(&dir.join("questions.jsonl"))?;
    let mut files: Vec<_> = std::fs::read_dir(dir.join("results"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort_by_key(|path| {
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        (name != "anchor", !name.starts_with("baseline-"), name)
    });
    let mut out = Vec::new();
    for path in files {
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let results: Vec<SystemResult> = read_jsonl(&path)?;
        out.push((name, score(&questions, &results)));
    }
    Ok(out)
}

fn cell(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
}

/// Renders the scoreboard, one table per category.
pub fn render(board: &[(String, BTreeMap<String, Score>)]) -> String {
    let mut out = String::new();
    for category in ["ownership", "justification", "relationships", "all"] {
        let Some(n) = board
            .iter()
            .find_map(|(_, scores)| scores.get(category).map(|s| s.questions))
        else {
            continue;
        };
        out.push_str(&format!(
            "\n{category} ({n} questions)\n{:<22} {:>8} {:>9} {:>8} {:>10} {:>8} {:>9}\n",
            "system", "ctx-rec", "ctx-full", "own-ans", "gen-ans", "tokens", "ms/query"
        ));
        for (name, scores) in board {
            let Some(score) = scores.get(category) else {
                continue;
            };
            out.push_str(&format!(
                "{:<22} {:>8.2} {:>9.2} {:>8} {:>10} {:>8.0} {:>9.1}\n",
                name,
                score.context_recall,
                score.context_complete,
                cell(score.answer_recall),
                cell(score.generated_recall),
                score.context_tokens,
                score.latency_ms
            ));
        }
    }
    if let Some(n) = board
        .iter()
        .find_map(|(_, scores)| scores.get("all").map(|s| s.stale_questions))
        .filter(|n| *n > 0)
    {
        out.push_str(&format!(
            "\nconflicting sources ({n} questions with a stale claim)\n{:<22} {:>13} {:>13}\n",
            "system", "ctx-not-stale", "gen-not-stale"
        ));
        for (name, scores) in board {
            if let Some(score) = scores.get("all") {
                out.push_str(&format!(
                    "{:<22} {:>13.2} {:>13}\n",
                    name,
                    score.stale_context_safe,
                    cell(score.stale_generated_safe)
                ));
            }
        }
    }
    out.push_str(
        "\nctx-rec: share of expected answers found in the retrieved text. ctx-full: questions \
         with every expected answer retrieved.\nown-ans: the system's own answer (no model). \
         gen-ans: answer written from the contexts by the shared generator model.\n\
         ctx-not-stale / gen-not-stale: the stale claim is absent or comes after the correct one.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(expected: &[&[&str]], stale: &[&[&str]], phrases: bool) -> Question {
        let groups = |g: &[&[&str]]| -> Vec<Vec<String>> {
            g.iter()
                .map(|forms| forms.iter().map(|f| (*f).to_owned()).collect())
                .collect()
        };
        Question {
            id: "q".into(),
            category: "ownership".into(),
            kind: "direct".into(),
            question: "Who owns it?".into(),
            expected: groups(expected),
            stale: groups(stale),
            phrases,
        }
    }

    fn result(contexts: &[&str], generated: Option<&str>) -> SystemResult {
        SystemResult {
            id: "q".into(),
            contexts: contexts.iter().map(|c| (*c).to_owned()).collect(),
            answer: None,
            generated_answer: generated.map(str::to_owned),
            latency_ms: 1.0,
        }
    }

    #[test]
    fn matching_respects_word_boundaries_and_punctuation() {
        let text = normalize("Owned by @acme/Payments-Core (see team:payments-core).");
        assert!(find(&text, &["payments core".into()]).is_some());
        assert!(find(&text, &["Payments-Core".into()]).is_some());
        assert!(find(&text, &["payments".into()]).is_some());
        assert!(find(&text, &["pay".into()]).is_none());
        assert!(find(&text, &["ments core".into()]).is_none());
    }

    #[test]
    fn recall_counts_groups_not_forms() {
        let q = question(&[&["alice", "Alice Smith"], &["bob"]], &[], false);
        let scores = score(&[q], &[result(&["alice smith, also known as alice"], None)]);
        assert!((scores["all"].context_recall - 0.5).abs() < 1e-9);
        assert!(scores["all"].context_complete.abs() < 1e-9);
        assert_eq!(scores["all"].generated_recall, None);
    }

    #[test]
    fn stale_claims_are_safe_only_after_the_correct_answer() {
        let q = question(&[&["core team"]], &[&["legacy team"]], false);
        let lead_correct = result(
            &["core team owns it", "legacy team owned it"],
            Some("The Core Team."),
        );
        let lead_stale = result(
            &["legacy team owns it", "core team owns it"],
            Some("Legacy team."),
        );
        let good = score(std::slice::from_ref(&q), &[lead_correct]);
        let bad = score(&[q], &[lead_stale]);
        assert!((good["all"].stale_context_safe - 1.0).abs() < 1e-9);
        assert_eq!(good["all"].stale_generated_safe, Some(1.0));
        assert!(bad["all"].stale_context_safe.abs() < 1e-9);
        assert_eq!(bad["all"].stale_generated_safe, Some(0.0));
    }

    #[test]
    fn generated_phrases_may_be_paraphrased_but_contexts_may_not() {
        let q = question(
            &[&["Out of scope for this runbook; tracked as a separate design note"]],
            &[],
            true,
        );
        let paraphrase = "It was dismissed as out of scope for the runbook and is tracked in a separate design note.";
        let scores = score(&[q], &[result(&[paraphrase], Some(paraphrase))]);
        assert!(scores["all"].context_recall.abs() < 1e-9);
        assert_eq!(scores["all"].generated_recall, Some(1.0));
    }

    #[test]
    fn contexts_are_cut_to_the_shared_budget() {
        let q = question(&[&["needle"]], &[], false);
        let filler = "hay ".repeat(CONTEXT_TOKENS);
        let scores = score(&[q], &[result(&[&filler, &filler, "needle"], None)]);
        assert!(scores["all"].context_recall.abs() < 1e-9);
        assert!(scores["all"].context_tokens <= CONTEXT_TOKENS as f64 * 1.01);
    }
}
