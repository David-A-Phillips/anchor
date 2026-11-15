//! The query engine: intent → seeds → trust-weighted traversal → context → tiered synthesis.
//!
//! Retrieval never needs a language model: intent parsing falls back to heuristics, and ownership
//! and justification questions are answered directly from the graph. A model (System 1 or 2, as
//! the router decides) is only used to synthesise prose for complex or low-confidence questions.

mod config;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::embed::VectorIndex;
use super::graph::{
    Direction, Goal, GraphConfig, Limits, Path, Pathfinder, Seed, SharedGraph, TraversalProfile,
};
use super::router::{RetrievalSignals, TieredRouter};
use super::store::{GraphStore, NodeRecord, Resolution};
use crate::backlog::{Backlog, GapReport};
use crate::base::{
    CallPurpose, ChunkId, CompletionRequest, EdgeKind, EmbedKind, Embedder, FinishReason,
    IntentKind, Lookup, ModelTier, NodeId, NodeKind, QueryIntent, Result, TrustLevel, attrs,
};

pub use config::QueryConfig;

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
}

impl QueryRequest {
    /// A request in [`QueryMode::Auto`] with default limits.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            mode: QueryMode::Auto,
            top_k: None,
            max_context_tokens: None,
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
    /// Gaps found.
    pub gaps: Vec<Gap>,
    /// Model calls made.
    pub usage: Vec<CallUsage>,
    /// Stage timings in milliseconds.
    pub timings_ms: BTreeMap<String, u64>,
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
}

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
    /// Builds the engine.
    pub fn new(deps: QueryDeps) -> Self {
        Self { deps }
    }

    fn node_ref(&self, id: NodeId) -> Result<Option<NodeRef>> {
        Ok(self.deps.store.node(id)?.as_ref().map(NodeRef::from))
    }

    fn snippets(&self, id: NodeId, max: usize) -> Result<Vec<Snippet>> {
        Ok(self
            .deps
            .store
            .chunks_of(id)?
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

    /// Blocking retrieval: seeds, traversal, conflict resolution, context assembly.
    fn retrieve_blocking(
        &self,
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

        // 1. Seeds.
        let mut seeds: Vec<Seed> = Vec::new();
        let mut exact = false;
        let add = |node: NodeId, cost: f32, seeds: &mut Vec<Seed>| {
            if !seeds.iter().any(|seed| seed.node == node) {
                seeds.push(Seed { node, cost });
            }
        };
        for mention in &intent.entities {
            let kinds: Vec<NodeKind> = match mention.kind_hint {
                Some(kind) => std::iter::once(kind)
                    .chain(ENTITY_KINDS.iter().copied().filter(move |k| *k != kind))
                    .collect(),
                None => ENTITY_KINDS.to_vec(),
            };
            match store.resolve(&Lookup::new(&kinds, mention.text.clone()))? {
                Resolution::Resolved(node) => {
                    exact = true;
                    add(node, 0.0, &mut seeds);
                }
                Resolution::Ambiguous(nodes) => {
                    for node in nodes.into_iter().take(3) {
                        add(node, 0.5, &mut seeds);
                    }
                }
                Resolution::NotFound => {
                    let hits = store.search_nodes(&mention.text, &[], 3)?;
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
        if seeds.is_empty() {
            for hit in store.search_nodes(&request.query, &[], 3)? {
                add(hit.node, 1.0, &mut seeds);
            }
            for hit in text_hits.iter().take(3) {
                add(hit.node, 1.5, &mut seeds);
            }
        }

        // 2. Traverse.
        let profile = TraversalProfile::for_intent(intent.kind);
        let limits = Limits::from(&self.deps.graph_config);
        let seed_ids: HashSet<NodeId> = seeds.iter().map(|seed| seed.node).collect();
        let (paths, reached) = {
            let graph = self.deps.graph.read();
            let finder = Pathfinder::new(&graph, &self.deps.graph_config);
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
            (paths, reached)
        };
        let paths: Vec<Path> = paths
            .into_iter()
            .filter(|path| {
                path.target().is_some_and(|node| !seed_ids.contains(&node)) || path.steps.is_empty()
            })
            .collect();

        // 3. Assemble.
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
            let snippets = this.snippets(node, 2)?;
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

        let single_valued = intent.kind == IntentKind::Ownership;
        let winner_authoritative = paths
            .first()
            .is_some_and(|path| path.cost.is_authoritative());
        let mut overridden_refs = Vec::new();
        for (rank, path) in paths.iter().enumerate() {
            let Some(target) = path.target() else {
                continue;
            };
            let overridden =
                single_valued && rank > 0 && winner_authoritative && !path.cost.is_authoritative();
            let role = if overridden {
                ItemRole::Overridden
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
        for seed in &seeds {
            push(self, seed.node, ItemRole::Evidence, 0.9, None, &mut bundle)?;
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
        let confidence = confidence.clamp(0.0, 1.0);

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

        let direct_answer = self.direct_answer(intent, &seeds, &paths, &bundle)?;
        Ok(Retrieved {
            bundle,
            confidence,
            gaps,
            reports,
            direct_answer,
            authoritative: winner_authoritative,
        })
    }

    /// Composes an answer from the graph alone, when the question allows it.
    fn direct_answer(
        &self,
        intent: &QueryIntent,
        seeds: &[Seed],
        paths: &[Path],
        bundle: &ContextBundle,
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
                        let reason = edge
                            .claims
                            .iter()
                            .find_map(|claim| {
                                claim.attrs.get(attrs::REASON).and_then(|v| v.as_str())
                            })
                            .or_else(|| other.attrs.get(attrs::REASON).and_then(|v| v.as_str()))
                            .or_else(|| {
                                other
                                    .attrs
                                    .get(attrs::CLOSE_REASON)
                                    .and_then(|v| v.as_str())
                            })
                            .map(str::to_owned);
                        let state = other
                            .attrs
                            .get(attrs::DECISION)
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
                    return Ok(None);
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
            && let Ok(hits) = vectors.search_async(vector, 6, None).await
        {
            vector_hits = hits.into_iter().map(|hit| hit.id).collect();
        }

        let stage = Instant::now();
        let this = self.clone();
        let (req, int) = (request.clone(), intent.clone());
        let mut retrieved =
            tokio::task::spawn_blocking(move || this.retrieve_blocking(&req, &int, &vector_hits))
                .await??;
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
            route.reason = format!("{:?}", decision.reason);
            answer = retrieved.direct_answer.take();
            let needs_model = answer.is_none() || tier == Some(ModelTier::System2);
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
            gaps: retrieved.gaps,
            usage,
            timings_ms: timings,
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
