//! Recall, precision and relationship accuracy on the synthetic enterprise.
//!
//! The dataset is `anchor::synth` (small preset, fixed seed), ingested with the real engine. For
//! every ground-truth question the query engine runs its retrieval stage (`QueryEngine::retrieve`,
//! no model involved) with the default `top_k`, and the returned [`ContextBundle`] is scored.
//! Keys are compared as node identities: a ground-truth key and a context item match when
//! `store.node_by_key(key)` returns the item's node id, so secondary (merged) keys count.
//!
//! Definitions, per question:
//!
//! * **Context Recall** = |expected keys present among the context items| / |expected keys|.
//!   "Expected keys" are `expected_owner_keys` (ownership) or `expected_keys` (justification and
//!   relationships). A context item of any role counts as retrieved.
//! * **Context Precision** = |answer-role items that are expected| / |answer-role items|, where
//!   answer-role items are the context items with `ItemRole::Answer`. For ownership questions the
//!   expected set also includes the team leads (`expected_lead_keys`), because "who should I
//!   contact" answers name them. A question that retrieves no answer-role item adds nothing to
//!   either count of the precision fraction.
//!
//! Category numbers are micro-averaged over all questions of the category (sum of numerators over
//! sum of denominators).
//!
//! * **Ownership accuracy** = share of ownership questions whose top answer-role item (highest
//!   score) is an expected owner. In conflict scenarios the stale claimant must never be the top
//!   answer.
//! * **Multi-hop relationship accuracy** = share of relationship questions whose declared paths
//!   all exist in the graph and whose expected keys were all retrieved.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anchor::base::{NodeId, NodeKey};
use anchor::config::AnchorConfig;
use anchor::engine::Engine;
use anchor::engine::query::{
    ContextBundle, ItemRole, QueryDeps, QueryEngine, QueryMode, QueryRequest,
};
use anchor::synth::{
    self, GroundTruth, OwnershipScenario, Preset, RelationshipQuestion, SynthSpec,
};

const SEED: u64 = 2024;

struct World {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    queries: QueryEngine,
    truth: GroundTruth,
}

fn build_world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let spec = SynthSpec::preset(Preset::Small).seed(SEED);
    synth::generate(&spec, dir.path()).unwrap();
    let config = load_config(dir.path());
    let engine = Engine::open(config).unwrap();
    let reports = engine.scan_all().unwrap();
    assert!(
        reports.iter().all(|r| r.warnings.is_empty()),
        "ingest warnings: {reports:#?}"
    );
    let truth: GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();
    let queries = QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: Some(engine.vectors()),
        embedder: Some(engine.embedder()),
        router: engine.router(),
        backlog: Some(engine.backlog()),
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    });
    World {
        _dir: dir,
        engine,
        queries,
        truth,
    }
}

fn load_config(root: &Path) -> AnchorConfig {
    AnchorConfig::from_toml_str(&std::fs::read_to_string(root.join("anchor.toml")).unwrap())
        .unwrap()
}

impl World {
    fn id(&self, key: &str) -> Option<NodeId> {
        self.engine
            .store()
            .node_by_key(&NodeKey::new(key))
            .unwrap()
            .map(|node| node.id)
    }

    fn ids(&self, keys: &[String]) -> HashSet<NodeId> {
        keys.iter()
            .map(|key| self.id(key).unwrap_or_else(|| panic!("unknown key {key}")))
            .collect()
    }

    async fn retrieve(&self, question: &str) -> ContextBundle {
        let mut request = QueryRequest::new(question);
        request.mode = QueryMode::RetrievalOnly;
        self.queries.retrieve(&request).await.unwrap().1
    }
}

fn all_ids(bundle: &ContextBundle) -> HashSet<NodeId> {
    bundle.items.iter().map(|i| NodeId(i.node.id)).collect()
}

fn answer_ids(bundle: &ContextBundle) -> Vec<NodeId> {
    bundle
        .items
        .iter()
        .filter(|i| i.role == ItemRole::Answer)
        .map(|i| NodeId(i.node.id))
        .collect()
}

#[derive(Default, Debug, Clone, Copy)]
struct Score {
    found: usize,
    expected: usize,
    correct_answers: usize,
    answers: usize,
    questions: usize,
}

impl Score {
    fn add(&mut self, expected: &HashSet<NodeId>, precise: &HashSet<NodeId>, b: &ContextBundle) {
        let retrieved = all_ids(b);
        self.found += expected.intersection(&retrieved).count();
        self.expected += expected.len();
        let answers = answer_ids(b);
        self.correct_answers += answers.iter().filter(|id| precise.contains(id)).count();
        self.answers += answers.len();
        self.questions += 1;
    }
    fn recall(&self) -> f64 {
        self.found as f64 / self.expected.max(1) as f64
    }
    fn precision(&self) -> f64 {
        self.correct_answers as f64 / self.answers.max(1) as f64
    }
}

fn report(label: &str, score: &Score) {
    println!(
        "{label:<34} questions {:>3}  recall {:>6.3} ({}/{})  precision {:>6.3} ({}/{})",
        score.questions,
        score.recall(),
        score.found,
        score.expected,
        score.precision(),
        score.correct_answers,
        score.answers
    );
}

struct Thresholds {
    ownership_recall: f64,
    ownership_precision: f64,
    ownership_accuracy: f64,
    justification_recall: f64,
    justification_precision: f64,
    relationship_recall: f64,
    relationship_accuracy: f64,
}

const THRESHOLDS: Thresholds = Thresholds {
    ownership_recall: 0.95,
    ownership_precision: 0.15,
    ownership_accuracy: 0.90,
    justification_recall: 0.30,
    justification_precision: 0.03,
    relationship_recall: 0.90,
    relationship_accuracy: 0.90,
};

fn declared_paths_exist(world: &World, q: &RelationshipQuestion) -> bool {
    let store = world.engine.store();
    q.paths.iter().all(|path| {
        path.windows(2).all(|pair| {
            let kind = pair[1].edge.as_deref().unwrap();
            let (Some(a), Some(b)) = (world.id(&pair[0].key), world.id(&pair[1].key)) else {
                return false;
            };
            store.edges_of(a, 100_000).unwrap().iter().any(|edge| {
                let (from, to) = if pair[1].forward { (a, b) } else { (b, a) };
                edge.from == from && edge.to == to && format!("{:?}", edge.kind) == kind
            })
        })
    })
}

#[test]
fn recall_precision_and_accuracy_on_the_synthetic_enterprise() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let world = build_world();
    let truth = &world.truth;
    println!(
        "dataset: seed {SEED}, {} ownership, {} justification, {} relationship questions, {:?}",
        truth.ownership.len(),
        truth.justification.len(),
        truth.relationships.len(),
        world.engine.store().stats().unwrap().nodes
    );
    assert!(!truth.ownership.is_empty() && !truth.justification.is_empty());

    // --- ownership ---------------------------------------------------------------------------
    let mut ownership = Score::default();
    let mut by_kind: BTreeMap<String, Score> = BTreeMap::new();
    let mut by_scenario: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let (mut top_ok, mut conflicts, mut stale_wins, mut conflict_ok) = (0, 0, 0, 0);
    let mut misses = Vec::new();
    for q in &truth.ownership {
        let bundle = runtime.block_on(world.retrieve(&q.question));
        let owners = world.ids(&q.expected_owner_keys);
        let mut precise = owners.clone();
        precise.extend(world.ids(&q.expected_lead_keys));
        ownership.add(&owners, &precise, &bundle);
        by_kind
            .entry(q.kind.as_str().to_owned())
            .or_default()
            .add(&owners, &precise, &bundle);
        let top = bundle
            .items
            .iter()
            .filter(|i| i.role == ItemRole::Answer)
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .map(|i| NodeId(i.node.id));
        let ok = top.is_some_and(|id| owners.contains(&id));
        let entry = by_scenario
            .entry(format!("{:?}", q.scenario))
            .or_insert((0, 0));
        entry.1 += 1;
        if ok {
            top_ok += 1;
            entry.0 += 1;
        } else {
            misses.push(format!("{} [{:?}] {}", q.id, q.scenario, q.question));
        }
        if q.scenario == OwnershipScenario::Conflict {
            conflicts += 1;
            let stale = world.ids(&q.stale_claim_keys);
            if top.is_some_and(|id| stale.contains(&id)) {
                stale_wins += 1;
            }
            if ok {
                conflict_ok += 1;
            }
        }
    }
    println!("--- context recall / precision ---");
    report("ownership (all)", &ownership);
    for (kind, score) in &by_kind {
        report(&format!("  ownership.{kind}"), score);
    }
    let accuracy = top_ok as f64 / truth.ownership.len() as f64;
    println!(
        "ownership accuracy (top answer is an expected owner): {accuracy:.3} ({top_ok}/{})",
        truth.ownership.len()
    );
    for (scenario, (ok, total)) in &by_scenario {
        println!("  scenario {scenario:<14} {ok}/{total}");
    }
    println!(
        "conflict scenarios: {conflict_ok}/{conflicts} correct, stale claim won {stale_wins} times"
    );
    for miss in &misses {
        println!("  top answer wrong: {miss}");
    }

    // --- justification -----------------------------------------------------------------------
    let mut justification = Score::default();
    let mut just_kind: BTreeMap<String, Score> = BTreeMap::new();
    for q in &truth.justification {
        let bundle = runtime.block_on(world.retrieve(&q.question));
        let expected = world.ids(&q.expected_keys);
        justification.add(&expected, &expected, &bundle);
        just_kind
            .entry(q.kind.as_str().to_owned())
            .or_default()
            .add(&expected, &expected, &bundle);
    }
    report("justification (all)", &justification);
    for (kind, score) in &just_kind {
        report(&format!("  justification.{kind}"), score);
    }

    // --- relationships -----------------------------------------------------------------------
    let mut relationships = Score::default();
    let mut rel_kind: BTreeMap<String, Score> = BTreeMap::new();
    let (mut rel_ok, mut paths_ok) = (0, 0);
    for q in &truth.relationships {
        let bundle = runtime.block_on(world.retrieve(&q.question));
        let expected = world.ids(&q.expected_keys);
        relationships.add(&expected, &expected, &bundle);
        rel_kind
            .entry(q.kind.as_str().to_owned())
            .or_default()
            .add(&expected, &expected, &bundle);
        let declared = declared_paths_exist(&world, q);
        paths_ok += usize::from(declared);
        if declared && expected.is_subset(&all_ids(&bundle)) {
            rel_ok += 1;
        }
    }
    report("relationships (all)", &relationships);
    for (kind, score) in &rel_kind {
        report(&format!("  relationship.{kind}"), score);
    }
    let total_rel = truth.relationships.len();
    let rel_acc = rel_ok as f64 / total_rel as f64;
    println!(
        "multi-hop: declared paths exist {paths_ok}/{total_rel}; paths exist and all expected keys retrieved {rel_ok}/{total_rel} = {rel_acc:.3}"
    );

    // --- thresholds (set below the measured values, see the printed numbers) -------------------
    let t = &THRESHOLDS;
    assert_eq!(paths_ok, total_rel, "every declared path must exist");
    assert_eq!(stale_wins, 0, "a stale claim won a conflict");
    assert!(ownership.recall() >= t.ownership_recall, "ownership recall");
    assert!(
        ownership.precision() >= t.ownership_precision,
        "ownership precision"
    );
    assert!(accuracy >= t.ownership_accuracy, "ownership accuracy");
    assert!(
        justification.recall() >= t.justification_recall,
        "justification recall"
    );
    assert!(
        justification.precision() >= t.justification_precision,
        "justification precision"
    );
    assert!(
        relationships.recall() >= t.relationship_recall,
        "relationship recall"
    );
    // Relationship questions are read as relation chains and followed hop by hop, so they
    // produce answer-role items.
    assert!(relationships.answers > 0, "no relationship answers");
    // Categories the engine handles well must stay well handled.
    for kind in ["api", "service", "endpoint", "file"] {
        assert!(by_kind[kind].recall() >= 0.95, "ownership.{kind} recall");
    }
    assert!(just_kind["api_not_approved"].recall() >= 0.9);
    assert!(just_kind["endpoint_blocked"].recall() >= 0.9);
    assert!(rel_kind["service_portfolio"].recall() >= 0.95);
    assert!(rel_acc >= t.relationship_accuracy, "relationship accuracy");
}
