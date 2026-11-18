//! The query engine: intent → seeds → trust-weighted traversal → context → tiered synthesis.
//!
//! Retrieval never needs a language model: intent parsing falls back to heuristics, and ownership
//! and justification questions are answered directly from the graph. A model (System 1 or 2, as
//! the router decides) is only used to synthesise prose for complex or low-confidence questions.

mod config;
pub mod fusion;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::embed::VectorIndex;
use super::graph::{
    ContextGraph, Direction, Goal, GraphConfig, Limits, Path, PathCost, Pathfinder, Seed,
    SharedGraph, TraversalProfile,
};
use super::router::{RetrievalSignals, TieredRouter, relation};
use super::store::{GraphStore, NodeRecord, Resolution};
use crate::backlog::{Backlog, GapReport};
use crate::base::{
    CallPurpose, ChunkId, CompletionRequest, EdgeKind, EmbedKind, Embedder, FinishReason,
    IntentKind, Lookup, ModelTier, NodeId, NodeKind, QueryIntent, Result, SourceId, TrustLevel,
    attrs,
};
use crate::config::Depth;

pub use config::{QueryConfig, ShallowPolicy};

/// Lets the query engine see which sources are shallow and ask for them to be deepened.
/// Implemented by the engine; a remote implementation can pull deeper tiles from a server.
pub trait DepthControl: Send + Sync {
    /// Sources ingested below full depth.
    fn shallow_sources(&self) -> Vec<(SourceId, Depth)>;
    /// Brings a source to full depth. Blocking.
    fn deepen(&self, source: &SourceId) -> Result<()>;
}

/// A part of the answer that rests on a source ingested below full depth.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShallowArea {
    /// The shallow source.
    pub source: String,
    /// Depth it was at when the question was asked.
    pub depth: Depth,
    /// `deepened` (done before answering), `deepening` (in progress in the background) or
    /// `manual` (waiting for `anchor deepen`).
    pub status: String,
    /// What this means for the answer.
    pub detail: String,
}

/// How much of the pipeline to run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Retrieve, then let the router pick a tier (or none).
    #[default]
    Auto,
    /// Never call the cloud tier.
    LocalOnly,
    /// Retrieval only: no model call, no answer.
    RetrievalOnly,
}

/// A question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The question text.
    pub query: String,
    /// Pipeline mode.
    #[serde(default)]
    pub mode: QueryMode,
    /// Maximum context items; defaults to [`QueryConfig::top_k`].
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Context token budget; defaults to [`QueryConfig::max_context_tokens`].
    #[serde(default)]
    pub max_context_tokens: Option<usize>,
    /// Ask a model to write the answer even when [`QueryConfig::model_answers`] is off.
    #[serde(default)]
    pub explain: bool,
    /// Retrieval time budget in milliseconds; defaults to [`QueryConfig::timeout_ms`].
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl QueryRequest {
    /// A request in [`QueryMode::Auto`] with default limits.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            mode: QueryMode::Auto,
            top_k: None,
            max_context_tokens: None,
            explain: false,
            timeout_ms: None,
        }
    }
}

/// A node as it appears in responses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRef {
    /// Row id.
    pub id: i64,
    /// Primary key.
    pub key: String,
    /// Kind.
    pub kind: NodeKind,
    /// Display name.
    pub name: String,
    /// Trust level.
    pub trust: TrustLevel,
}

impl From<&NodeRecord> for NodeRef {
    fn from(node: &NodeRecord) -> Self {
        Self {
            id: node.id.0,
            key: node.key.to_string(),
            kind: node.kind,
            name: node.name.clone(),
            trust: node.trust,
        }
    }
}

/// A quoted piece of text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snippet {
    /// Chunk id.
    pub chunk_id: i64,
    /// Chunk kind.
    pub kind: String,
    /// The text (possibly truncated).
    pub text: String,
    /// Source that asserted it.
    pub source: String,
    /// Trust of that source.
    pub trust: TrustLevel,
}

/// One edge of an evidence path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathStep {
    /// Node the step leaves.
    pub from: NodeRef,
    /// Edge kind.
    pub edge: EdgeKind,
    /// Direction travelled.
    pub direction: Direction,
    /// Trust of the edge.
    pub trust: TrustLevel,
    /// Node the step enters.
    pub to: NodeRef,
}

/// Why an item is in the context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemRole {
    /// The answer to the question.
    Answer,
    /// Supporting evidence.
    Evidence,
    /// A claim overridden by a more authoritative one.
    Overridden,
}

/// One item of retrieved context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    /// The node.
    pub node: NodeRef,
    /// Why it is here.
    pub role: ItemRole,
    /// Relevance in `[0, 1]`.
    pub score: f32,
    /// Text attached to the node.
    pub snippets: Vec<Snippet>,
    /// How the node was reached from the question's entities.
    pub path: Vec<PathStep>,
}

impl ContextItem {
    /// The evidence path as text, e.g. `service:a <-owns- team:b`; empty when the node was
    /// found directly.
    pub fn path_text(&self) -> String {
        let Some(first) = self.path.first() else {
            return String::new();
        };
        let mut out = first.from.key.clone();
        for step in &self.path {
            if step.direction == Direction::Reverse {
                out.push_str(&format!(" <-{}- ", step.edge));
            } else {
                out.push_str(&format!(" -{}-> ", step.edge));
            }
            out.push_str(&step.to.key);
        }
        out
    }
}

/// An authoritative claim overriding less trusted ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    /// What the claims disagree about.
    pub question: String,
    /// The winning node.
    pub winner: NodeRef,
    /// The overridden nodes.
    pub overridden: Vec<NodeRef>,
    /// Human-readable explanation.
    pub explanation: String,
}

/// The retrieved context.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextBundle {
    /// Items, most useful first.
    pub items: Vec<ContextItem>,
    /// Detected conflicts.
    pub conflicts: Vec<Conflict>,
    /// Approximate size in tokens.
    pub tokens: usize,
}

/// A gap found while answering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    /// `low_confidence`, `missing_neighbor`, `unresolved_entity` or `conflict`.
    pub kind: String,
    /// What is missing.
    pub detail: String,
}

/// The routing outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteInfo {
    /// Tier that produced the answer; `None` when no model was used.
    pub tier: Option<ModelTier>,
    /// Why.
    pub reason: String,
    /// `true` when a fallback tier served the request.
    pub degraded: bool,
}

/// One model call made for the query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallUsage {
    /// Tier used.
    pub tier: ModelTier,
    /// Model name.
    pub model: String,
    /// Purpose of the call.
    pub purpose: CallPurpose,
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Latency in milliseconds.
    pub latency_ms: u64,
}

/// The answer to a [`QueryRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryResponse {
    /// Correlation id (also recorded in the token ledger).
    pub query_id: String,
    /// The question.
    pub query: String,
    /// Parsed intent.
    pub intent: QueryIntent,
    /// Routing outcome.
    pub route: RouteInfo,
    /// The answer, when one was produced.
    pub answer: Option<String>,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
    /// Retrieved context.
    pub context: ContextBundle,
    /// Sources below full depth that this answer rests on, or that might hold what is missing.
    /// Empty when everything relevant is fully ingested.
    #[serde(default)]
    pub shallow: Vec<ShallowArea>,
    /// Gaps found.
    pub gaps: Vec<Gap>,
    /// Model calls made.
    pub usage: Vec<CallUsage>,
    /// Stage timings in milliseconds.
    pub timings_ms: BTreeMap<String, u64>,
    /// `true` when graph traversal stopped early on its time or visit budget: the answer is
    /// valid but may be incomplete.
    #[serde(default)]
    pub truncated: bool,
}

/// What the query engine is built from.
#[derive(Clone)]
pub struct QueryDeps {
    /// The graph store.
    pub store: GraphStore,
    /// The in-memory graph.
    pub graph: SharedGraph,
    /// Vector index, if embeddings are in use.
    pub vectors: Option<Arc<VectorIndex>>,
    /// Embedder for the query text.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Model router.
    pub router: Arc<TieredRouter>,
    /// Backlog for gap analysis.
    pub backlog: Option<Arc<Backlog>>,
    /// Traversal limits and trust weights.
    pub graph_config: GraphConfig,
    /// Retrieval settings.
    pub query_config: QueryConfig,
}

/// The query engine. Cheap to clone.
#[derive(Clone)]
pub struct QueryEngine {
    deps: QueryDeps,
    depths: Option<Arc<dyn DepthControl>>,
}

/// Candidates kept for a name that several entities share.
const AMBIGUOUS_CANDIDATES: usize = 6;
/// Starting points taken from the fused search ranking when a question names nothing.
const SEARCH_SEEDS: usize = 6;
/// Nearest chunks asked of the vector index: more when a neural model's vectors are fused into
/// the ranking, fewer when hash vectors only add trailing evidence.
const VECTOR_HITS: usize = 12;
const HASH_VECTOR_HITS: usize = 6;

const ENTITY_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::ApiSpec,
    NodeKind::ApiEndpoint,
    NodeKind::Component,
    NodeKind::Domain,
    NodeKind::Portfolio,
    NodeKind::Ticket,
    NodeKind::PullRequest,
    NodeKind::Team,
    NodeKind::Person,
    NodeKind::WikiPage,
    NodeKind::Symbol,
    NodeKind::File,
];

struct Retrieved {
    bundle: ContextBundle,
    confidence: f32,
    gaps: Vec<Gap>,
    reports: Vec<GapReport>,
    direct_answer: Option<String>,
    authoritative: bool,
    truncated: bool,
}

/// The content words of a text, lower-cased and lightly stemmed, for matching a question
/// against candidate passages.
fn question_terms(text: &str) -> HashSet<String> {
    const COMMON: &[&str] = &[
        "what",
        "when",
        "where",
        "which",
        "does",
        "that",
        "this",
        "with",
        "from",
        "have",
        "should",
        "would",
        "could",
        "about",
        "there",
        "their",
        "they",
        "them",
        "then",
        "than",
        "into",
        "been",
        "being",
        "were",
        "will",
        "your",
        "ours",
        "supposed",
        "according",
    ];
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.len() >= 4)
        .map(str::to_lowercase)
        .filter(|word| !COMMON.contains(&word.as_str()))
        .map(|word| {
            let stem = word
                .strip_suffix("ing")
                .or_else(|| word.strip_suffix("ed"))
                .or_else(|| word.strip_suffix("es"))
                .or_else(|| word.strip_suffix('s'))
                .filter(|stem| stem.len() >= 4)
                .map(str::to_owned);
            stem.unwrap_or(word)
        })
        .collect()
}

/// "by <who>: " when the node records who closed or dismissed it.
fn closed_by(node: &NodeRecord) -> String {
    node.attrs
        .get(attrs::CLOSED_BY)
        .and_then(|v| v.as_str())
        .map(|who| format!("by {who}: "))
        .unwrap_or_default()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

impl QueryEngine {
    /// Builds the engine. The store and the graph are tied together so that retrieval reads both
    /// at the same version (see [`GraphStore::consistent`]).
    pub fn new(mut deps: QueryDeps) -> Self {
        match deps.store.mirror() {
            Some(mirror) => deps.graph = mirror.clone(),
            None => deps.store = deps.store.clone().with_mirror(deps.graph.clone()),
        }
        Self { deps, depths: None }
    }

    /// The deadline of a retrieval that starts now.
    fn deadline(&self, request: &QueryRequest) -> Option<Instant> {
        let millis = request
            .timeout_ms
            .unwrap_or(self.deps.query_config.timeout_ms);
        (millis > 0).then(|| Instant::now() + Duration::from_millis(millis))
    }

    /// Makes the engine depth-aware: answers that touch shallow sources are flagged, and
    /// [`QueryConfig::on_shallow`] decides whether to deepen first, in the background, or not.
    pub fn with_depths(mut self, depths: Arc<dyn DepthControl>) -> Self {
        self.depths = Some(depths);
        self
    }

    /// The shallow sources relevant to a retrieval: those that contributed context, plus every
    /// shallow source when the answer is weak (what is missing may be in any of them).
    fn shallow_for(&self, retrieved: &Retrieved) -> Result<Vec<(SourceId, Depth)>> {
        let Some(depths) = &self.depths else {
            return Ok(Vec::new());
        };
        let shallow = depths.shallow_sources();
        if shallow.is_empty() {
            return Ok(shallow);
        }
        let weak = retrieved.confidence < self.deps.query_config.confidence_threshold
            || retrieved.gaps.iter().any(|gap| gap.kind != "conflict");
        if weak {
            return Ok(shallow);
        }
        let mut used = HashSet::new();
        for item in &retrieved.bundle.items {
            if let Some(node) = self.deps.store.node(NodeId(item.node.id))? {
                used.extend(node.sources);
            }
        }
        Ok(shallow
            .into_iter()
            .filter(|(source, _)| used.contains(source))
            .collect())
    }

    fn node_ref(&self, id: NodeId) -> Result<Option<NodeRef>> {
        Ok(self.deps.store.node(id)?.as_ref().map(NodeRef::from))
    }

    /// Up to `max` pieces of the node's text: those sharing the most words with the question,
    /// in document order (a long page is quoted where it is relevant, not from its top).
    fn snippets(&self, id: NodeId, max: usize, terms: &HashSet<String>) -> Result<Vec<Snippet>> {
        let mut chunks = self.deps.store.chunks_of(id)?;
        if chunks.len() > max && !terms.is_empty() {
            let mut scored: Vec<(usize, usize)> = chunks
                .iter()
                .enumerate()
                .map(|(index, chunk)| {
                    let words = question_terms(&chunk.text);
                    (terms.intersection(&words).count(), index)
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut keep: Vec<usize> = scored.into_iter().take(max).map(|(_, i)| i).collect();
            keep.sort_unstable();
            let mut index = 0;
            chunks.retain(|_| {
                index += 1;
                keep.contains(&(index - 1))
            });
        }
        Ok(chunks
            .into_iter()
            .take(max)
            .map(|chunk| Snippet {
                chunk_id: chunk.id.0,
                kind: chunk.kind.as_str().to_owned(),
                text: truncate(&chunk.text, 600),
                source: chunk.source.to_string(),
                trust: chunk.trust,
            })
            .collect())
    }

    fn steps(&self, path: &Path) -> Result<Vec<PathStep>> {
        let mut out = Vec::with_capacity(path.steps.len());
        for (index, step) in path.steps.iter().enumerate() {
            let (Some(from), Some(to)) = (
                self.node_ref(path.nodes[index])?,
                self.node_ref(path.nodes[index + 1])?,
            ) else {
                continue;
            };
            out.push(PathStep {
                from,
                edge: step.kind,
                direction: step.direction,
                trust: step.trust,
                to,
            });
        }
        Ok(out)
    }

    /// Blocking retrieval: seeds, traversal, conflict resolution, context assembly, all against
    /// one consistent version of the graph and its tables.
    fn retrieve_blocking(
        &self,
        request: &QueryRequest,
        intent: &QueryIntent,
        vector_hits: &[ChunkId],
    ) -> Result<Retrieved> {
        let deadline = self.deadline(request);
        self.deps
            .store
            .consistent(|graph| self.retrieve_in(graph, deadline, request, intent, vector_hits))
    }

    fn vector_hits(&self) -> usize {
        if self.semantic_vectors() {
            VECTOR_HITS
        } else {
            HASH_VECTOR_HITS
        }
    }

    /// `true` when the vectors come from a neural embedding model rather than feature hashing.
    fn semantic_vectors(&self) -> bool {
        self.deps
            .embedder
            .as_ref()
            .is_some_and(|embedder| !embedder.model_id().starts_with("hash"))
    }

    fn retrieve_in(
        &self,
        graph: &ContextGraph,
        deadline: Option<Instant>,
        request: &QueryRequest,
        intent: &QueryIntent,
        vector_hits: &[ChunkId],
    ) -> Result<Retrieved> {
        let store = &self.deps.store;
        let top_k = request.top_k.unwrap_or(self.deps.query_config.top_k).max(1);
        let budget = request
            .max_context_tokens
            .unwrap_or(self.deps.query_config.max_context_tokens);
        let mut gaps = Vec::new();
        let mut reports = Vec::new();

        // 1. Seeds. References of the form `repo#123` or `#123` name an issue, discussion or
        // pull request exactly; put them first so they anchor the search.
        let mut entities: Vec<crate::base::EntityMention> = request
            .query
            .split(|ch: char| ch.is_whitespace() || matches!(ch, '?' | ',' | ';' | '(' | ')' | '"'))
            .map(|token| token.trim_end_matches(['.', ':', '!']))
            .filter(|token| {
                token.rsplit_once('#').is_some_and(|(_, number)| {
                    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
                })
            })
            .map(crate::base::EntityMention::new)
            .collect();
        for mention in &intent.entities {
            if !entities
                .iter()
                .any(|known| known.text.contains(&mention.text))
            {
                entities.push(mention.clone());
            }
        }
        let mut seeds: Vec<Seed> = Vec::new();
        let mut exact = false;
        let mut ambiguous: Vec<(String, usize)> = Vec::new();
        let add = |node: NodeId, cost: f32, seeds: &mut Vec<Seed>| {
            if !seeds.iter().any(|seed| seed.node == node) {
                seeds.push(Seed { node, cost });
            }
        };
        // Resolve every mention first: a name shared by many entities (`src/lib.rs`, `POST
        // /v1/refunds`, a sentence quoted from several pages) is then narrowed to the candidates
        // that belong to an entity the question names unambiguously ("... in billing-api").
        let mut resolved = Vec::with_capacity(entities.len());
        let mut scopes: Vec<String> = Vec::new();
        for mention in &entities {
            let kinds: Vec<NodeKind> = match mention.kind_hint {
                Some(kind) => std::iter::once(kind)
                    .chain(ENTITY_KINDS.iter().copied().filter(move |k| *k != kind))
                    .collect(),
                None => ENTITY_KINDS.to_vec(),
            };
            let resolution = store.resolve(&Lookup::new(&kinds, mention.text.clone()))?;
            if let Resolution::Resolved(node) = &resolution
                && let Some(node) = store.node(*node)?
            {
                for key in &node.keys {
                    if let Some((_, slug)) = key.as_str().rsplit_once(':')
                        && slug.len() > 2
                    {
                        scopes.push(slug.to_lowercase());
                    }
                }
            }
            resolved.push((mention, resolution));
        }
        // A candidate is in scope when its key has a named entity's slug as a whole segment
        // (`file:billing-api:src/lib.rs`); failing that for every candidate, when a segment
        // starts with the slug (`thread:wiki:billing-api-runbook/t-1`).
        let in_scope = |nodes: Vec<NodeId>| -> Result<Vec<NodeId>> {
            if scopes.is_empty() || nodes.len() < 2 {
                return Ok(nodes);
            }
            let (mut whole, mut prefix) = (Vec::new(), Vec::new());
            for node in &nodes {
                let Some(record) = store.node(*node)? else {
                    continue;
                };
                let key = record.key.as_str().to_lowercase();
                let mut best = 0;
                for scope in &scopes {
                    for (at, _) in key.match_indices(scope.as_str()) {
                        if !matches!(key[..at].chars().next_back(), Some(':' | '/')) {
                            continue;
                        }
                        match key[at + scope.len()..].chars().next() {
                            None | Some(':' | '/' | '#') => best = 2,
                            Some('-' | '_' | '.') => best = best.max(1),
                            _ => {}
                        }
                    }
                }
                match best {
                    2 => whole.push(*node),
                    1 => prefix.push(*node),
                    _ => {}
                }
            }
            Ok(if !whole.is_empty() {
                whole
            } else if !prefix.is_empty() {
                prefix
            } else {
                nodes
            })
        };
        for (mention, resolution) in resolved {
            match resolution {
                Resolution::Resolved(node) => {
                    exact = true;
                    add(node, 0.0, &mut seeds);
                }
                Resolution::Ambiguous(nodes) => {
                    let nodes = in_scope(nodes)?;
                    if let [node] = nodes[..] {
                        exact = true;
                        add(node, 0.0, &mut seeds);
                    } else {
                        ambiguous.push((mention.text.clone(), nodes.len()));
                        for node in nodes.into_iter().take(AMBIGUOUS_CANDIDATES) {
                            add(node, 0.5, &mut seeds);
                        }
                    }
                }
                Resolution::NotFound => {
                    let hits = store.search_nodes(&mention.text, &[], 12)?;
                    let best = hits.first().map_or(0.0, |hit| hit.score);
                    let near: Vec<NodeId> = hits
                        .iter()
                        .filter(|hit| hit.score >= best - 0.05)
                        .map(|hit| hit.node)
                        .collect();
                    let near = in_scope(near)?;
                    if near.len() > 1 && best >= 0.7 {
                        ambiguous.push((mention.text.clone(), near.len()));
                    }
                    let hits: Vec<_> = hits
                        .into_iter()
                        .filter(|hit| near.contains(&hit.node))
                        .take(AMBIGUOUS_CANDIDATES)
                        .collect();
                    if hits.iter().all(|hit| hit.score < 0.7) {
                        gaps.push(Gap {
                            kind: "unresolved_entity".into(),
                            detail: format!("nothing in the graph is called `{}`", mention.text),
                        });
                        reports.push(GapReport::UnresolvedEntity {
                            mention: mention.text.clone(),
                            query: Some(request.query.clone()),
                        });
                    }
                    for hit in hits {
                        add(hit.node, 1.0 - hit.score.clamp(0.0, 1.0) + 0.3, &mut seeds);
                    }
                }
            }
        }
        let text_hits = store.search_chunks(&request.query, top_k)?;
        // Nothing named and no kind of answer sought (an open question looking for passages):
        // start from what the searches find, fused into one ranking, and let relevance flow one
        // step along the graph's links (see `fusion`).
        let mut searched: Vec<(NodeId, f32)> = Vec::new();
        // A question looking for a kind of answer (an owner, a dependency) is answered from the
        // nearest such node to its best name match, so its starting points keep that order.
        if seeds.is_empty() && !intent.target_kinds.is_empty() {
            for hit in store.search_nodes(&request.query, &[], 3)? {
                add(hit.node, 1.0, &mut seeds);
            }
            for hit in text_hits.iter().take(3) {
                add(hit.node, 1.5, &mut seeds);
            }
        }
        if seeds.is_empty() {
            let dedup = |nodes: &mut dyn Iterator<Item = NodeId>| {
                let mut seen = HashSet::new();
                nodes.filter(|node| seen.insert(*node)).collect::<Vec<_>>()
            };
            // Only names the question actually contains: a name sharing one common word with
            // it ("the", "are") says nothing about relevance.
            let mut named = Vec::new();
            for hit in store.search_nodes(&request.query, &[], 20)? {
                if let Some(node) = store.node(hit.node)?
                    && fusion::names(&request.query, &node.name)
                {
                    named.push(hit.node);
                }
            }
            let by_name = dedup(&mut named.into_iter());
            let by_text = dedup(&mut text_hits.iter().map(|hit| hit.node));
            let mut rankings = vec![by_name, by_text];
            // Hash vectors restate the full-text search; only a neural model's vectors add a
            // ranking of their own.
            if self.semantic_vectors() {
                let mut by_vector = Vec::new();
                for chunk in vector_hits {
                    if let Some(chunk) = store.chunk(*chunk)? {
                        by_vector.push(chunk.node);
                    }
                }
                rankings.push(dedup(&mut by_vector.into_iter()));
            }
            let fused = fusion::rrf(&rankings, fusion::RRF_K);
            for (rank, (node, _)) in fused.iter().take(SEARCH_SEEDS).enumerate() {
                add(*node, 1.0 + 0.1 * rank as f32, &mut seeds);
            }
            searched = fusion::spread(
                &fused,
                |node| {
                    graph
                        .neighbors(node)
                        .map(|step| (step.node, step.edge.weight))
                        .collect::<Vec<_>>()
                },
                fusion::SPREAD_FROM,
                fusion::SPREAD,
            );
        }

        // 2. Traverse.
        let profile = TraversalProfile::for_intent(intent.kind);
        let limits = Limits::from(&self.deps.graph_config).until(deadline);
        let seed_ids: HashSet<NodeId> = seeds.iter().map(|seed| seed.node).collect();
        let (paths, reached, mut truncated) = {
            let finder = Pathfinder::new(graph, &self.deps.graph_config);
            let paths = if intent.target_kinds.is_empty() {
                Vec::new()
            } else {
                finder.nearest(
                    &seeds,
                    &Goal::kinds(&intent.target_kinds),
                    top_k,
                    &profile,
                    limits,
                )
            };
            let reached = finder.expand(&seeds, &profile, limits, top_k * 4);
            (paths, reached, finder.truncated())
        };
        let paths: Vec<Path> = paths
            .into_iter()
            .filter(|path| {
                path.target().is_some_and(|node| !seed_ids.contains(&node)) || path.steps.is_empty()
            })
            .collect();
        // A question that names its own path through the graph is answered by following it.
        let chain = relation::parse(&request.query);
        let followed = match &chain {
            Some(chain) => {
                let (followed, cut) = self.follow(graph, chain, &seeds, limits);
                truncated |= cut;
                followed
            }
            None => Vec::new(),
        };
        let chain = chain.filter(|_| !followed.is_empty());
        let paths = if chain.is_some() { followed } else { paths };

        // 3. Assemble.
        let terms = question_terms(&request.query);
        // Pages written as the documentation of a named entity (its runbook, its design note)
        // are where "how do I ..." and open questions about it are answered.
        let mut documentation: Vec<(usize, NodeId)> = Vec::new();
        if matches!(
            intent.kind,
            IntentKind::HowTo | IntentKind::General | IntentKind::Definition
        ) && chain.is_none()
        {
            for seed in seeds.iter().filter(|seed| seed.cost <= 0.5).take(3) {
                for edge in store.edges_of(seed.node, 200)? {
                    if edge.kind != EdgeKind::Documents || edge.to != seed.node {
                        continue;
                    }
                    let overlap = store
                        .chunks_of(edge.from)?
                        .iter()
                        .map(|chunk| terms.intersection(&question_terms(&chunk.text)).count())
                        .max()
                        .unwrap_or(0);
                    if !documentation.iter().any(|(_, node)| *node == edge.from) {
                        documentation.push((overlap, edge.from));
                    }
                }
            }
            documentation.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            documentation.truncate(3);
        }
        let mut bundle = ContextBundle::default();
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut push = |this: &Self,
                        node: NodeId,
                        role: ItemRole,
                        score: f32,
                        path: Option<&Path>,
                        bundle: &mut ContextBundle|
         -> Result<()> {
            if !seen.insert(node) {
                return Ok(());
            }
            let Some(record) = this.deps.store.node(node)? else {
                return Ok(());
            };
            let snippets = this.snippets(node, 2, &terms)?;
            let cost: usize = snippets.iter().map(|s| s.text.len() / 4).sum::<usize>() + 24;
            if bundle.tokens + cost > budget && !bundle.items.is_empty() {
                return Ok(());
            }
            bundle.tokens += cost;
            bundle.items.push(ContextItem {
                node: NodeRef::from(&record),
                role,
                score,
                snippets,
                path: match path {
                    Some(path) => this.steps(path)?,
                    None => Vec::new(),
                },
            });
            Ok(())
        };

        let single_valued = intent.kind == IntentKind::Ownership && chain.is_none();
        let winner_authoritative = paths
            .first()
            .is_some_and(|path| path.cost.is_authoritative());
        let mut overridden_refs = Vec::new();
        // For a single-valued question, the nearest owner of each subject is the answer (with any
        // others at the same distance and trust, e.g. two CODEOWNERS); owners further away, such as
        // the team owning the enclosing component or the domain lead, are evidence.
        let mut nearest: HashMap<NodeId, (usize, bool)> = HashMap::new();
        for (rank, path) in paths.iter().enumerate() {
            let Some(target) = path.target() else {
                continue;
            };
            let overridden =
                single_valued && rank > 0 && winner_authoritative && !path.cost.is_authoritative();
            let shape = (path.steps.len(), path.cost.is_authoritative());
            let secondary = single_valued
                && !path.steps.is_empty()
                && path
                    .nodes
                    .first()
                    .is_some_and(|seed| *nearest.entry(*seed).or_insert(shape) != shape);
            let role = if overridden {
                ItemRole::Overridden
            } else if secondary {
                ItemRole::Evidence
            } else {
                ItemRole::Answer
            };
            if overridden && let Some(node) = self.node_ref(target)? {
                overridden_refs.push(node);
            }
            push(
                self,
                target,
                role,
                1.0 / (1.0 + rank as f32),
                Some(path),
                &mut bundle,
            )?;
        }
        if !overridden_refs.is_empty()
            && let Some(winner) = paths.first().and_then(Path::target)
            && let Some(winner) = self.node_ref(winner)?
        {
            let names: Vec<&str> = overridden_refs.iter().map(|n| n.name.as_str()).collect();
            let explanation = format!(
                "{} is backed by authoritative sources; {} is only claimed by reference-only sources, so the authoritative claim takes precedence",
                winner.name,
                names.join(", ")
            );
            gaps.push(Gap {
                kind: "conflict".into(),
                detail: explanation.clone(),
            });
            if let Some(seed) = seeds
                .first()
                .and_then(|seed| self.node_ref(seed.node).ok().flatten())
            {
                reports.push(GapReport::Conflict {
                    node_key: seed.key,
                    topic: intent.kind.as_str().to_owned(),
                    authoritative: winner.key.clone(),
                    reference: overridden_refs
                        .iter()
                        .map(|n| n.key.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
            bundle.conflicts.push(Conflict {
                question: format!(
                    "{} of {}",
                    intent.kind,
                    intent.entities.first().map_or("?", |e| e.text.as_str())
                ),
                winner,
                overridden: overridden_refs,
                explanation,
            });
        }
        for (node, score) in searched.iter().take(top_k) {
            push(
                self,
                *node,
                ItemRole::Evidence,
                0.9 * score,
                None,
                &mut bundle,
            )?;
        }
        for seed in &seeds {
            push(self, seed.node, ItemRole::Evidence, 0.9, None, &mut bundle)?;
        }
        for (_, page) in &documentation {
            push(self, *page, ItemRole::Evidence, 0.85, None, &mut bundle)?;
        }
        for path in &paths {
            for node in &path.nodes {
                push(self, *node, ItemRole::Evidence, 0.6, None, &mut bundle)?;
            }
        }
        for item in reached.iter().filter(|item| item.hops > 0).take(top_k) {
            push(
                self,
                item.node,
                ItemRole::Evidence,
                0.5 / (1.0 + item.hops as f32),
                None,
                &mut bundle,
            )?;
        }
        for hit in text_hits.iter().take(top_k / 2 + 1) {
            push(self, hit.node, ItemRole::Evidence, 0.4, None, &mut bundle)?;
        }
        for chunk in vector_hits {
            if let Some(chunk) = store.chunk(*chunk)? {
                push(
                    self,
                    chunk.node,
                    ItemRole::Evidence,
                    0.35,
                    None,
                    &mut bundle,
                )?;
            }
        }
        bundle.items.truncate(top_k.max(paths.len()) + top_k);

        // 4. Confidence.
        let found_target = paths.iter().any(|path| !path.steps.is_empty());
        let mut confidence: f32 = 0.15;
        if !seeds.is_empty() {
            confidence += 0.1;
        }
        if exact {
            confidence += 0.3;
        }
        if intent.target_kinds.is_empty() {
            confidence += if bundle.items.len() > 1 { 0.2 } else { 0.0 };
        } else if found_target {
            confidence += 0.3;
            if winner_authoritative {
                confidence += 0.15;
            }
        }
        let mut confidence = confidence.clamp(0.0, 1.0);
        // The question names something the graph does not hold, and nothing else it names was
        // found: whatever the fuzzy matches lead to is about something else. Report the gap and
        // the evidence, never an answer about a different entity.
        let subject_missing = !exact && gaps.iter().any(|gap| gap.kind == "unresolved_entity");
        if subject_missing {
            confidence = confidence.min(0.3);
        }

        if intent.kind == IntentKind::Ownership && !found_target && exact {
            let key = seeds
                .first()
                .and_then(|seed| self.node_ref(seed.node).ok().flatten());
            if let Some(node) = key {
                gaps.push(Gap {
                    kind: "missing_neighbor".into(),
                    detail: format!("{} has no owner in the graph", node.name),
                });
                reports.push(GapReport::MissingNeighbor {
                    node_key: node.key,
                    relation: "owner".into(),
                    note: None,
                });
            }
        }

        let direct_answer = match &chain {
            _ if subject_missing => None,
            Some(chain) => self.chain_answer(chain, &paths)?,
            None => self.direct_answer(intent, &seeds, &paths, &bundle, &ambiguous)?,
        };
        if subject_missing {
            for item in &mut bundle.items {
                if item.role == ItemRole::Answer {
                    item.role = ItemRole::Evidence;
                }
            }
        }
        if truncated {
            gaps.push(Gap {
                kind: "truncated".into(),
                detail: "graph traversal stopped at its time or visit budget; the answer may be incomplete, so name the entity more precisely or raise `[query] timeout_ms` / `[graph] max_visits`".into(),
            });
        }
        for (name, count) in &ambiguous {
            gaps.push(Gap {
                kind: "ambiguous_entity".into(),
                detail: format!(
                    "{count} entities are called `{name}`; name the service or repository to narrow it down"
                ),
            });
        }
        Ok(Retrieved {
            bundle,
            confidence,
            gaps,
            reports,
            direct_answer,
            authoritative: winner_authoritative,
            truncated,
        })
    }

    /// Follows a relation chain from the question's entities, hop by hop. Each hop keeps the
    /// most trustworthy nearest matches (or all of them, for hops such as "consumers"), and the
    /// returned paths run from the starting entity to each final answer. Empty when any hop
    /// leads nowhere. The flag is `true` when a hop's search was cut short.
    fn follow(
        &self,
        graph: &ContextGraph,
        chain: &relation::Chain,
        seeds: &[Seed],
        limits: Limits,
    ) -> (Vec<Path>, bool) {
        let best = seeds
            .iter()
            .map(|seed| seed.cost)
            .fold(f32::INFINITY, f32::min);
        let mut frontier: Vec<Path> = seeds
            .iter()
            .filter(|seed| seed.cost <= best)
            .map(|seed| Path {
                nodes: vec![seed.node],
                steps: Vec::new(),
                cost: PathCost::ZERO,
            })
            .collect();
        let finder = Pathfinder::new(graph, &self.deps.graph_config);
        for hop in &chain.hops {
            let starts: Vec<Seed> = frontier
                .iter()
                .filter_map(Path::target)
                .map(Seed::new)
                .collect();
            let mut goal = Goal::kinds(&hop.targets());
            goal.exclude = starts.iter().map(|seed| seed.node).collect();
            let found = finder.nearest(&starts, &goal, 50, &hop.profile(), limits);
            let Some(first) = found.first() else {
                return (Vec::new(), finder.truncated());
            };
            let (top, fewest) = (
                first.cost,
                found.iter().map(|p| p.steps.len()).min().unwrap_or(0),
            );
            let mut next = Vec::new();
            for path in found {
                let keep = if hop.collects_all() {
                    path.steps.len() == fewest
                } else {
                    path.cost.reference_elements == top.reference_elements
                        && path.cost.weighted <= top.weighted + 1e-3
                };
                let prefix = path
                    .nodes
                    .first()
                    .and_then(|start| frontier.iter().find(|p| p.target() == Some(*start)));
                if let (true, Some(prefix)) = (keep, prefix) {
                    let mut joined = prefix.clone();
                    joined.nodes.extend(path.nodes.iter().skip(1));
                    joined.steps.extend(path.steps.iter().copied());
                    joined.cost += path.cost;
                    next.push(joined);
                }
            }
            frontier = next;
        }
        frontier.retain(|path| !path.steps.is_empty());
        (frontier, finder.truncated())
    }

    /// The answer to a relation-chain question: the entities reached, how trustworthy the path
    /// is, and what it passed through.
    fn chain_answer(&self, chain: &relation::Chain, paths: &[Path]) -> Result<Option<String>> {
        let Some(first) = paths.first() else {
            return Ok(None);
        };
        let Some(subject) = self.node_ref(first.nodes[0])? else {
            return Ok(None);
        };
        let mut names = Vec::new();
        for path in paths {
            if let Some(node) = path
                .target()
                .map(|n| self.node_ref(n))
                .transpose()?
                .flatten()
                && !names.contains(&node.name)
            {
                names.push(node.name);
            }
        }
        if names.is_empty() {
            return Ok(None);
        }
        let basis = if paths.iter().all(|path| path.cost.is_authoritative()) {
            "source of truth"
        } else {
            "partly from reference-only sources"
        };
        let mut via = Vec::new();
        if first.nodes.len() > 2 {
            for node in &first.nodes[1..first.nodes.len() - 1] {
                if let Some(node) = self.node_ref(*node)? {
                    via.push(node.name);
                }
            }
        }
        let mut answer = format!(
            "{} of {}: {} [{}]",
            chain.label(),
            subject.name,
            names.join(", "),
            basis
        );
        if !via.is_empty() {
            answer.push_str(&format!(" (via {})", via.join(" → ")));
        }
        answer.push('.');
        Ok(Some(answer))
    }

    /// Composes an answer from the graph alone, when the question allows it.
    fn direct_answer(
        &self,
        intent: &QueryIntent,
        seeds: &[Seed],
        paths: &[Path],
        bundle: &ContextBundle,
        ambiguous: &[(String, usize)],
    ) -> Result<Option<String>> {
        let subject = match seeds.first() {
            Some(seed) => self.node_ref(seed.node)?,
            None => None,
        };
        let Some(subject) = subject else {
            return Ok(None);
        };
        match intent.kind {
            IntentKind::Ownership => {
                // Several entities share the name and nothing in the question says which one is
                // meant: answer for each candidate instead of silently picking one.
                if let Some((name, count)) = ambiguous.first()
                    && seeds.iter().all(|seed| seed.cost > 0.0)
                {
                    let mut owners = Vec::new();
                    for seed in seeds {
                        let Some(path) = paths.iter().find(|path| {
                            path.nodes.first() == Some(&seed.node) && !path.steps.is_empty()
                        }) else {
                            continue;
                        };
                        if let (Some(node), Some(owner)) = (
                            self.node_ref(seed.node)?,
                            path.target()
                                .map(|n| self.node_ref(n))
                                .transpose()?
                                .flatten(),
                        ) {
                            owners.push(format!("{} is owned by {}", node.key, owner.name));
                        }
                    }
                    if owners.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(format!(
                        "`{name}` is ambiguous: {count} entities have that name. {}{}. Name the service or repository for a single answer.",
                        owners.join("; "),
                        if *count > owners.len() { "; ..." } else { "" }
                    )));
                }
                let Some(path) = paths.iter().find(|path| !path.steps.is_empty()) else {
                    return Ok(None);
                };
                let Some(owner) = path
                    .target()
                    .map(|node| self.node_ref(node))
                    .transpose()?
                    .flatten()
                else {
                    return Ok(None);
                };
                let via: Vec<String> = self
                    .steps(path)?
                    .iter()
                    .rev()
                    .skip(1)
                    .map(|step| step.to.name.clone())
                    .collect();
                let basis = if path.cost.is_authoritative() {
                    "source of truth"
                } else {
                    "reference-only sources; no authoritative owner is recorded"
                };
                let mut answer = format!("{} is owned by {} [{}]", subject.name, owner.name, basis);
                if !via.is_empty() {
                    answer.push_str(&format!(" (via {})", via.join(" → ")));
                }
                answer.push('.');
                for conflict in &bundle.conflicts {
                    answer.push_str(&format!(" Note: {}.", conflict.explanation));
                }
                Ok(Some(answer))
            }
            IntentKind::Justification | IntentKind::Status => {
                let mut reasons = Vec::new();
                for seed in seeds.iter().take(3) {
                    for edge in self.deps.store.edges_of(seed.node, 200)? {
                        let blocks_seed = edge.kind == EdgeKind::Blocks && edge.to == seed.node;
                        let about_seed =
                            matches!(edge.kind, EdgeKind::Approves | EdgeKind::Discusses)
                                && edge.to == seed.node;
                        if !(blocks_seed || about_seed) {
                            continue;
                        }
                        let Some(other) = self.deps.store.node(edge.from)? else {
                            continue;
                        };
                        let stated = edge
                            .claims
                            .iter()
                            .find_map(|claim| {
                                claim.attrs.get(attrs::REASON).and_then(|v| v.as_str())
                            })
                            .or_else(|| other.attrs.get(attrs::REASON).and_then(|v| v.as_str()));
                        let closed = other
                            .attrs
                            .get(attrs::CLOSE_REASON)
                            .and_then(|v| v.as_str());
                        // Something closed or dismissed explains itself by why it was closed;
                        // what it originally said comes second.
                        let reason = match (closed, stated) {
                            (Some(closed), Some(stated)) => Some(format!(
                                "{}{closed} (it had said: {stated})",
                                closed_by(&other)
                            )),
                            (Some(closed), None) => Some(format!("{}{closed}", closed_by(&other))),
                            (None, stated) => stated.map(str::to_owned),
                        };
                        let state = other
                            .attrs
                            .get(attrs::THREAD_STATUS)
                            .filter(|v| v.as_str() == Some(attrs::THREAD_DISMISSED))
                            .or_else(|| other.attrs.get(attrs::DECISION))
                            .or_else(|| other.attrs.get(attrs::THREAD_STATUS))
                            .or_else(|| other.attrs.get(attrs::STATUS))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if blocks_seed || reason.is_some() || state == attrs::THREAD_DISMISSED {
                            let verb = if blocks_seed {
                                "blocked by"
                            } else {
                                "discussed in"
                            };
                            reasons.push(format!(
                                "{verb} {} ({}{}){}",
                                other.name,
                                other.kind,
                                if state.is_empty() {
                                    String::new()
                                } else {
                                    format!(", {state}")
                                },
                                reason.map(|r| format!(": {r}")).unwrap_or_default()
                            ));
                        }
                    }
                }
                if reasons.is_empty() {
                    // Nothing blocks it: report its recorded state, if it has one.
                    let Some(node) = self.deps.store.node(NodeId(subject.id))? else {
                        return Ok(None);
                    };
                    let state = [attrs::THREAD_STATUS, attrs::STATUS, attrs::DECISION]
                        .iter()
                        .find_map(|name| node.attrs.get(*name).and_then(|v| v.as_str()));
                    let reason = node
                        .attrs
                        .get(attrs::CLOSE_REASON)
                        .or_else(|| node.attrs.get(attrs::REASON))
                        .and_then(|v| v.as_str());
                    return Ok(state.map(|state| match reason {
                        Some(reason) => format!(
                            "{} ({}) is {state}: {}{reason}",
                            subject.name,
                            subject.kind,
                            closed_by(&node)
                        ),
                        None => format!("{} ({}) is {state}.", subject.name, subject.kind),
                    }));
                }
                Ok(Some(format!("{} is {}.", subject.name, reasons.join("; "))))
            }
            _ => Ok(None),
        }
    }

    /// Retrieval without synthesis (used by recall/precision benchmarks).
    pub async fn retrieve(
        &self,
        request: &QueryRequest,
    ) -> Result<(QueryIntent, ContextBundle, f32)> {
        let intent = self.deps.router.classify(&request.query, None).await;
        let this = self.clone();
        let (request, intent2) = (request.clone(), intent.clone());
        let retrieved =
            tokio::task::spawn_blocking(move || this.retrieve_blocking(&request, &intent2, &[]))
                .await??;
        Ok((intent, retrieved.bundle, retrieved.confidence))
    }

    /// Answers a question.
    pub async fn query(&self, request: QueryRequest) -> Result<QueryResponse> {
        let started = Instant::now();
        let query_id = uuid::Uuid::now_v7().to_string();
        let mut timings = BTreeMap::new();
        let mut usage = Vec::new();

        let intent = self
            .deps
            .router
            .classify(&request.query, Some(&query_id))
            .await;
        timings.insert("classify".to_owned(), started.elapsed().as_millis() as u64);

        let mut vector_hits = Vec::new();
        if let (Some(vectors), Some(embedder)) = (&self.deps.vectors, &self.deps.embedder)
            && !vectors.is_empty()
            && let Ok(mut embedded) = embedder
                .embed(std::slice::from_ref(&request.query), EmbedKind::Query)
                .await
            && let Some(vector) = embedded.pop()
            && let Ok(hits) = vectors.search_async(vector, self.vector_hits(), None).await
        {
            vector_hits = hits.into_iter().map(|hit| hit.id).collect();
        }

        let stage = Instant::now();
        let this = self.clone();
        let (req, int) = (request.clone(), intent.clone());
        let mut retrieved =
            tokio::task::spawn_blocking(move || this.retrieve_blocking(&req, &int, &vector_hits))
                .await??;
        // Shallow areas: flag them, and deepen according to the policy.
        let mut shallow = Vec::new();
        if let Some(depths) = self.depths.clone() {
            let this = self.clone();
            let (relevant, back) = tokio::task::spawn_blocking(move || {
                let relevant = this.shallow_for(&retrieved);
                (relevant, retrieved)
            })
            .await?;
            retrieved = back;
            let relevant = relevant?;
            let policy = self.deps.query_config.on_shallow;
            if !relevant.is_empty() {
                let sources: Vec<SourceId> =
                    relevant.iter().map(|(source, _)| source.clone()).collect();
                let status = match policy {
                    ShallowPolicy::Block => {
                        let control = depths.clone();
                        let todo = sources.clone();
                        tokio::task::spawn_blocking(move || {
                            todo.iter().try_for_each(|source| control.deepen(source))
                        })
                        .await??;
                        let this = self.clone();
                        let (req, int) = (request.clone(), intent.clone());
                        retrieved = tokio::task::spawn_blocking(move || {
                            this.retrieve_blocking(&req, &int, &[])
                        })
                        .await??;
                        "deepened"
                    }
                    ShallowPolicy::Background => {
                        let control = depths.clone();
                        tokio::task::spawn_blocking(move || {
                            for source in &sources {
                                if let Err(err) = control.deepen(source) {
                                    tracing::warn!("deepening {source} failed: {err}");
                                }
                            }
                        });
                        "deepening"
                    }
                    ShallowPolicy::Manual => "manual",
                };
                for (source, depth) in relevant {
                    let detail = match status {
                        "deepened" => format!(
                            "{source} was at {depth} depth and has been deepened to full before answering"
                        ),
                        "deepening" => format!(
                            "{source} is at {depth} depth: this answer may be incomplete and will improve once background deepening finishes"
                        ),
                        _ => format!(
                            "{source} is at {depth} depth: this answer may be incomplete; run `anchor deepen {source}` for full detail"
                        ),
                    };
                    shallow.push(ShallowArea {
                        source: source.to_string(),
                        depth,
                        status: status.to_owned(),
                        detail,
                    });
                }
            }
        }
        timings.insert("retrieve".to_owned(), stage.elapsed().as_millis() as u64);

        let stage = Instant::now();
        let mut route = RouteInfo {
            tier: None,
            reason: "retrieval only".into(),
            degraded: false,
        };
        let mut answer = None;
        if request.mode != QueryMode::RetrievalOnly {
            let signals = RetrievalSignals {
                confidence: retrieved.confidence,
                has_conflict: !retrieved.bundle.conflicts.is_empty(),
                authoritative: retrieved.authoritative,
                context_tokens: retrieved.bundle.tokens as u32,
            };
            let decision = self.deps.router.decide(&intent, &signals);
            let tier = match (decision.tier, request.mode) {
                (Some(ModelTier::System2), QueryMode::LocalOnly) => Some(ModelTier::System1),
                (tier, _) => tier,
            };
            route.reason = decision.reason.to_string();
            answer = retrieved.direct_answer.take();
            // Models write answers only when asked to: retrieval and direct answers never wait
            // on one.
            let wanted = request.explain || self.deps.query_config.model_answers;
            let needs_model = wanted && (answer.is_none() || tier == Some(ModelTier::System2));
            if let (Some(tier), true) = (tier, needs_model && !retrieved.bundle.items.is_empty()) {
                let prompt = self.prompt(&request.query, &retrieved.bundle, answer.as_deref());
                let completion = CompletionRequest::prompt(CallPurpose::Synthesize, prompt)
                    .with_system("You answer questions about an engineering organisation using only the evidence given. Prefer SOURCE_OF_TRUTH evidence over REFERENCE_ONLY evidence, mention conflicts, cite node names, and say what is unknown.")
                    .with_query_id(query_id.clone());
                match self.deps.router.complete(tier, completion).await {
                    Ok(routed)
                        if routed.completion.finish_reason != FinishReason::Filtered
                            && !routed.completion.text.trim().is_empty() =>
                    {
                        usage.push(CallUsage {
                            tier: routed.tier,
                            model: routed.completion.model.clone(),
                            purpose: CallPurpose::Synthesize,
                            prompt_tokens: routed.completion.usage.prompt_tokens,
                            completion_tokens: routed.completion.usage.completion_tokens,
                            latency_ms: routed.completion.latency.as_millis() as u64,
                        });
                        route.tier = Some(routed.tier);
                        route.degraded = routed.degraded;
                        answer = Some(routed.completion.text.trim().to_owned());
                    }
                    Ok(_) => route
                        .reason
                        .push_str("; model declined, answered from retrieved facts"),
                    Err(err) => route.reason.push_str(&format!(
                        "; model unavailable ({err}), answered from retrieved facts"
                    )),
                }
            }
        }
        timings.insert("synthesize".to_owned(), stage.elapsed().as_millis() as u64);

        if retrieved.confidence < self.deps.query_config.confidence_threshold {
            retrieved.gaps.push(Gap {
                kind: "low_confidence".into(),
                detail: format!(
                    "confidence {:.2} is below the threshold",
                    retrieved.confidence
                ),
            });
            retrieved.reports.push(GapReport::LowConfidence {
                query: request.query.clone(),
                intent: intent.clone(),
                confidence: retrieved.confidence,
                retrieved: retrieved
                    .bundle
                    .items
                    .iter()
                    .map(|item| item.node.key.clone())
                    .collect(),
            });
        }
        if let Some(backlog) = self.deps.backlog.clone() {
            let reports = std::mem::take(&mut retrieved.reports);
            tokio::task::spawn_blocking(move || {
                for report in &reports {
                    if let Err(err) = backlog.record(report) {
                        tracing::warn!("recording a backlog item failed: {err}");
                    }
                }
            })
            .await?;
        }
        timings.insert("total".to_owned(), started.elapsed().as_millis() as u64);

        Ok(QueryResponse {
            query_id,
            query: request.query,
            intent,
            route,
            answer,
            confidence: retrieved.confidence,
            context: retrieved.bundle,
            shallow,
            gaps: retrieved.gaps,
            usage,
            timings_ms: timings,
            truncated: retrieved.truncated,
        })
    }

    fn prompt(&self, query: &str, bundle: &ContextBundle, draft: Option<&str>) -> String {
        let mut prompt = format!("Question: {query}\n\nEvidence:\n");
        for item in &bundle.items {
            prompt.push_str(&format!(
                "- [{}] {} `{}` ({:?}, {})\n",
                item.node.kind,
                item.node.name,
                item.node.key,
                item.role,
                item.node.trust.as_str().to_uppercase()
            ));
            for step in &item.path {
                prompt.push_str(&format!(
                    "    path: {} -{}-> {}\n",
                    step.from.name, step.edge, step.to.name
                ));
            }
            for snippet in &item.snippets {
                prompt.push_str(&format!(
                    "    {}: {}\n",
                    snippet.source,
                    snippet.text.replace('\n', " ")
                ));
            }
        }
        for conflict in &bundle.conflicts {
            prompt.push_str(&format!("Conflict: {}\n", conflict.explanation));
        }
        if let Some(draft) = draft {
            prompt.push_str(&format!("\nFacts derived from the graph: {draft}\n"));
        }
        prompt.push_str("\nAnswer concisely.");
        prompt
    }
}

crate::base::opaque_debug!(QueryDeps, QueryEngine);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{
        Db, DeviceId, EdgeAssertion, Hlc, MemoryUsageSink, NodeAssertion, SourceDescriptor,
        SourceId, SourceKind, Unit, UnitKind, UnitVersion, keys,
    };
    use crate::engine::graph::ContextGraph;
    use crate::engine::router::RoutingConfig;

    fn engine() -> (QueryEngine, Arc<Backlog>) {
        let db = Db::open_in_memory().expect("db");
        let store = GraphStore::new(db.clone()).expect("store");
        for (id, kind, trust) in [
            ("repo", SourceKind::Repo, TrustLevel::SourceOfTruth),
            ("wiki", SourceKind::Wiki, TrustLevel::ReferenceOnly),
            ("jira", SourceKind::Jira, TrustLevel::ReferenceOnly),
        ] {
            store
                .upsert_source(&SourceDescriptor {
                    id: SourceId::new(id),
                    kind,
                    name: id.into(),
                    trust,
                })
                .expect("source");
        }
        let version = UnitVersion::new(Hlc::new(1, 0), DeviceId::new("d"));
        let mut repo = Unit::new("repo/ownership/CODEOWNERS", "repo", UnitKind::Ownership);
        let api = repo.add_node(NodeAssertion::new(
            NodeKind::ApiSpec,
            keys::api_spec("payments-api"),
            "Payments API",
        ));
        let team = repo.add_node(NodeAssertion::new(
            NodeKind::Team,
            keys::team("payments"),
            "Payments",
        ));
        repo.add_edge(EdgeAssertion::new(&team, EdgeKind::Owns, &api));
        let mut wiki = Unit::new("wiki/wiki_page/old", "wiki", UnitKind::WikiPage);
        let stale = wiki.add_node(NodeAssertion::new(
            NodeKind::Team,
            keys::team("checkout"),
            "Checkout",
        ));
        wiki.add_edge(EdgeAssertion::new(&stale, EdgeKind::Owns, &api));
        let mut jira = Unit::new("jira/ticket/PAY-2", "jira", UnitKind::Ticket);
        let blocked = jira.add_node(
            NodeAssertion::new(NodeKind::Ticket, keys::ticket("PAY-2"), "Ship v2").alias("PAY-2"),
        );
        let blocker = jira.add_node(
            NodeAssertion::new(NodeKind::Ticket, keys::ticket("PAY-1"), "Security review")
                .alias("PAY-1"),
        );
        jira.add_edge(
            EdgeAssertion::new(&blocker, EdgeKind::Blocks, &blocked)
                .attr(attrs::REASON, "pen test not finished"),
        );
        store
            .apply_units(&[repo, wiki, jira], &version)
            .expect("apply");
        let (nodes, edges) = store.load_graph().expect("graph");
        let backlog = Arc::new(Backlog::new(db, &Default::default()));
        let router = TieredRouter::new(
            None,
            None,
            Arc::new(MemoryUsageSink::new()),
            RoutingConfig::default(),
        );
        let engine = QueryEngine::new(QueryDeps {
            store,
            graph: ContextGraph::from_parts(nodes, edges).into_shared(),
            vectors: None,
            embedder: None,
            router: Arc::new(router),
            backlog: Some(backlog.clone()),
            graph_config: GraphConfig::default(),
            query_config: QueryConfig::default(),
        });
        (engine, backlog)
    }

    #[tokio::test]
    async fn authoritative_owner_wins_and_the_stale_claim_is_reported() {
        let (engine, _) = engine();
        let response = engine
            .query(QueryRequest::new("Who owns the payments-api API?"))
            .await
            .expect("query");
        assert_eq!(response.intent.kind, IntentKind::Ownership);
        let answer = response
            .answer
            .clone()
            .expect("direct answer without any model");
        assert!(
            answer.contains("Payments") && answer.contains("source of truth"),
            "{answer}"
        );
        assert_eq!(response.context.items[0].node.key, "team:payments");
        assert_eq!(response.context.items[0].role, ItemRole::Answer);
        assert_eq!(response.context.conflicts.len(), 1);
        assert_eq!(
            response.context.conflicts[0].overridden[0].key,
            "team:checkout"
        );
        assert!(response.route.tier.is_none() && response.usage.is_empty());
        assert!(response.confidence > 0.7);
        let json = serde_json::to_value(&response).expect("json");
        for field in [
            "query_id",
            "intent",
            "route",
            "answer",
            "confidence",
            "context",
            "gaps",
            "usage",
            "timings_ms",
        ] {
            assert!(json.get(field).is_some(), "missing {field}");
        }
    }

    #[tokio::test]
    async fn blocking_reason_is_explained_from_the_graph() {
        let (engine, _) = engine();
        let response = engine
            .query(QueryRequest::new("Why is PAY-2 blocked?"))
            .await
            .expect("query");
        let answer = response.answer.expect("answer");
        assert!(
            answer.contains("Security review") && answer.contains("pen test not finished"),
            "{answer}"
        );
        assert!(
            response
                .context
                .items
                .iter()
                .any(|item| item.node.key == "ticket:PAY-1")
        );
    }

    #[tokio::test]
    async fn unknown_entities_become_backlog_items() {
        let (engine, backlog) = engine();
        let response = engine
            .query(QueryRequest::new("Who owns the frobnicator-api API?"))
            .await
            .expect("query");
        assert!(
            response
                .gaps
                .iter()
                .any(|gap| gap.kind == "unresolved_entity" || gap.kind == "low_confidence")
        );
        assert!(backlog.stats().expect("stats").open >= 1);
        let retrieval = engine
            .retrieve(&QueryRequest::new("Who owns the payments-api API?"))
            .await
            .expect("retrieve");
        assert!(!retrieval.1.items.is_empty());
    }
}

#[cfg(test)]
mod behaviours;
#[cfg(test)]
mod behaviours_more;
