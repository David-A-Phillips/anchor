//! Functional tests: whole subsystems working together, with no network (in-process fake HTTP
//! servers on loopback only).
//!
//! 1. The ingestion pipeline: a multi-repository structure, an organisation model, a wiki with
//!    comment threads, an API portal and a Jira export are ingested into one store and graph;
//!    cross-source links, node and edge counts, re-scan idempotence and graph/store agreement are
//!    checked.
//! 2. Tiered routing and token-ledger accounting while toggling between a local Ollama endpoint
//!    and a cloud (Anthropic Messages API) endpoint.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anchor::base::{
    CallPurpose, CompletionRequest, EdgeKind, Locality, ModelError, ModelTier, NodeKey, SourceKind,
    TrustLevel, UsageSink, keys,
};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use anchor::engine::router::{
    ProviderKind, RetryPolicy, RoutingConfig, SpendTracker, TierConfig, TieredRouter,
};
use anchor::engine::{Engine, ScanReport};
use anchor::metrics::{PriceTable, TimeRange, TokenLedger};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};

// =============================================================================================
// 1. Ingestion pipeline
// =============================================================================================

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const ORG: &str = r#"
organisation: acme
portfolios:
  - name: Consumer Payments
    lead: asmith
    domains:
      - name: Billing
        components:
          - name: Ledger
            owner: payments-platform
            services: [payments-api, ledger-writer]
teams:
  - name: Payments Platform
    handle: payments-platform
    lead: asmith
    members: [asmith, bjones]
people:
  - { name: Alice Smith, handle: asmith, email: alice@acme.example }
  - { name: Bob Jones, handle: bjones }
"#;

const PORTAL: &str = r#"
id: API-1
name: payments-api
title: Payments API
version: "1.0"
status: in_review
owner_team: Payments Platform
implemented_by: [payments-api]
consumers: [ledger-writer]
spec:
  openapi: 3.0.0
  info: { title: Payments API, version: "1.0" }
  paths:
    /v1/charges:
      post: { operationId: createCharge, summary: Create a charge }
approvals:
  - id: APR-1
    approver: team:Security
    decision: rejected
    reason: Card numbers must not appear in query strings.
    scope: POST /v1/charges
"#;

const WIKI_RUNBOOK: &str = "---
id: payments-runbook
title: Payments API Runbook
author: Alice Smith <alice@acme.example>
---
# Payments API Runbook

The payments-api service retries failed charges. See PAY-1 for the retry work.
";

const WIKI_SIDECAR: &str = r#"{"threads": [{
  "id": "t-1", "kind": "inline", "status": "dismissed", "anchor": "retries failed charges",
  "closed_by": "Bob Jones", "close_reason": "Superseded by idempotency keys",
  "comments": [
    {"id": "c-1", "author": "Alice Smith", "body": "Should the payments-api retry forever?"},
    {"id": "c-2", "author": "Bob Jones", "body": "No, see PAY-1.", "reply_to": "c-1"}]}]}"#;

const WIKI_DESIGN: &str = r#"<html><head><title>Ledger Design</title><meta name="page-id" content="ledger-design"></head>
<body><main><h1>Ledger Design</h1><p>The ledger-writer service batches postings.</p></main>
<div class="comment-thread" data-thread-id="t-9" data-status="open">
  <div class="comment" data-comment-id="c-1" data-author="Bob Jones">Is batching safe?</div>
</div></body></html>"#;

const JIRA: &str = r#"{"issues": [
  {"key": "PAY-1", "fields": {"summary": "Charge retries in payments-api",
    "status": {"name": "In Progress"}, "components": [{"name": "payments-api"}],
    "issuelinks": [{"type": {"name": "Blocks", "inward": "is blocked by", "outward": "blocks"},
                    "outwardIssue": {"key": "PAY-2"}}]}},
  {"key": "PAY-2", "fields": {"summary": "Batch postings in ledger-writer",
    "status": {"name": "Blocked"}, "assignee": {"displayName": "Bob Jones"},
    "comment": {"comments": [{"id": "1", "author": {"displayName": "Alice Smith"},
                               "body": "Waiting for PAY-1."}]}}}]}"#;

fn lib_rs(name: &str) -> String {
    format!(
        "//! The {name} crate.\n\npub fn run(x: u32) -> u32 {{\n    helper(x) + 1\n}}\n\nfn helper(x: u32) -> u32 {{\n    x * 2\n}}\n"
    )
}

fn pipeline_config(root: &Path) -> AnchorConfig {
    write(root, "org/org.yaml", ORG);
    write(root, "portal/payments-api.yaml", PORTAL);
    write(root, "wiki/runbook.md", WIKI_RUNBOOK);
    write(root, "wiki/runbook.comments.json", WIKI_SIDECAR);
    write(root, "wiki/ledger-design.html", WIKI_DESIGN);
    write(root, "jira/issues.json", JIRA);
    write(
        root,
        "repos/payments-api/CODEOWNERS",
        "* @acme/payments-platform\n",
    );
    write(
        root,
        "repos/payments-api/README.md",
        "# payments-api\n\nCharges.\n",
    );
    write(
        root,
        "repos/payments-api/src/lib.rs",
        &lib_rs("payments-api"),
    );
    write(
        root,
        "repos/ledger-writer/CODEOWNERS",
        "* @acme/ledger-crew\n/src/ @bjones\n",
    );
    write(
        root,
        "repos/ledger-writer/src/lib.rs",
        &lib_rs("ledger-writer"),
    );

    let mut config = AnchorConfig {
        data_dir: Some(root.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    let mut add = |id: &str, kind: SourceKind, dir: &str, trust: TrustLevel| {
        let mut source = SourceConfig::new(id, kind, root.join(dir).to_string_lossy());
        source.trust = trust;
        config.sources.push(source);
    };
    use SourceKind::{ApiPortal, Jira, OrgModel, Repo, Wiki};
    add("org", OrgModel, "org", TrustLevel::SourceOfTruth);
    add(
        "payments-api",
        Repo,
        "repos/payments-api",
        TrustLevel::SourceOfTruth,
    );
    add(
        "ledger-writer",
        Repo,
        "repos/ledger-writer",
        TrustLevel::SourceOfTruth,
    );
    add("portal", ApiPortal, "portal", TrustLevel::SourceOfTruth);
    add("wiki", Wiki, "wiki", TrustLevel::ReferenceOnly);
    add("jira", Jira, "jira", TrustLevel::ReferenceOnly);
    config
}

struct Pipeline {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    reports: Vec<ScanReport>,
}

fn ingest_pipeline() -> Pipeline {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(pipeline_config(dir.path())).unwrap();
    let reports = engine.scan_all().unwrap();
    Pipeline {
        _dir: dir,
        engine,
        reports,
    }
}

impl Pipeline {
    fn id(&self, key: &NodeKey) -> anchor::base::NodeId {
        self.engine
            .store()
            .node_by_key(key)
            .unwrap()
            .unwrap_or_else(|| panic!("node {key} missing"))
            .id
    }

    /// `true` when an edge `from --kind--> to` is materialised.
    fn edge(&self, from: &NodeKey, kind: EdgeKind, to: &NodeKey) -> bool {
        let (a, b) = (self.id(from), self.id(to));
        self.engine
            .store()
            .edges_of(a, 100_000)
            .unwrap()
            .iter()
            .any(|e| e.from == a && e.to == b && e.kind == kind)
    }
}

#[test]
fn every_source_ingests_cleanly_into_one_store() {
    let p = ingest_pipeline();
    let warnings: Vec<String> = p
        .reports
        .iter()
        .flat_map(|r| r.warnings.iter().map(|w| format!("{}: {w}", r.source)))
        .collect();
    assert!(warnings.is_empty(), "{warnings:#?}");
    let scanned: Vec<&str> = p.reports.iter().map(|r| r.source.as_str()).collect();
    assert_eq!(
        scanned,
        [
            "org",
            "ledger-writer",
            "payments-api",
            "portal",
            "wiki",
            "jira"
        ],
        "scan order: org model first, then code, then the rest"
    );
    assert!(p.reports.iter().all(|r| r.units_applied > 0));
}

#[test]
fn cross_source_links_connect_wiki_tickets_apis_and_owners() {
    let p = ingest_pipeline();
    let source = anchor::base::SourceId::new("wiki");
    let (api_service, ledger_service) = (
        keys::service("payments-api"),
        keys::service("ledger-writer"),
    );
    let api = keys::api_spec("payments-api");

    // Wiki page -> service, and -> ticket, by name.
    let runbook = keys::wiki_page(&source, "payments-runbook");
    assert!(
        p.edge(&runbook, EdgeKind::Mentions, &api_service)
            || p.edge(&runbook, EdgeKind::Documents, &api_service),
        "wiki page links to the service it names"
    );
    assert!(p.edge(&runbook, EdgeKind::Mentions, &keys::ticket("PAY-1")));
    let design = keys::wiki_page(&source, "ledger-design");
    assert!(
        p.edge(&design, EdgeKind::Mentions, &ledger_service)
            || p.edge(&design, EdgeKind::Documents, &ledger_service)
    );

    // Tickets: Tracks (component field and summary) and Blocks (issue link).
    assert!(p.edge(&keys::ticket("PAY-1"), EdgeKind::Tracks, &api_service));
    assert!(p.edge(&keys::ticket("PAY-2"), EdgeKind::Tracks, &ledger_service));
    assert!(p.edge(
        &keys::ticket("PAY-1"),
        EdgeKind::Blocks,
        &keys::ticket("PAY-2")
    ));
    assert!(!p.edge(
        &keys::ticket("PAY-2"),
        EdgeKind::Blocks,
        &keys::ticket("PAY-1")
    ));

    // API: owned by the team, implemented and consumed by services, rejected by a review team.
    let platform = keys::team("payments-platform");
    assert!(p.edge(&platform, EdgeKind::Owns, &api), "API owner");
    assert!(p.edge(&api_service, EdgeKind::Implements, &api));
    assert!(p.edge(&ledger_service, EdgeKind::Consumes, &api));
    let approval = keys::approval(&anchor::base::SourceId::new("portal"), "APR-1");
    let endpoint = keys::api_endpoint("payments-api", "POST", "/v1/charges");
    assert!(p.edge(&approval, EdgeKind::Approves, &endpoint));
    assert!(p.edge(&approval, EdgeKind::Blocks, &endpoint));
    assert!(p.edge(&approval, EdgeKind::DecidedBy, &keys::team("Security")));

    // The org hierarchy.
    let (portfolio, domain, component) = (
        keys::portfolio("Consumer Payments"),
        keys::domain("Billing"),
        keys::component("Ledger"),
    );
    assert!(p.edge(&portfolio, EdgeKind::Contains, &domain));
    assert!(p.edge(&domain, EdgeKind::Contains, &component));
    assert!(p.edge(&component, EdgeKind::Contains, &api_service));
    assert!(p.edge(&platform, EdgeKind::Owns, &component));

    // The dismissed thread keeps its status, closer and reason; the closer is the same person
    // node as the org model's.
    let thread = keys::thread(&source, "payments-runbook/t-1");
    let record = p.engine.store().node_by_key(&thread).unwrap().unwrap();
    assert_eq!(record.attrs["thread_status"], "dismissed");
    assert_eq!(
        record.attrs["close_reason"],
        "Superseded by idempotency keys"
    );
    assert_eq!(record.trust, TrustLevel::ReferenceOnly);
    let bob = keys::person("bjones");
    assert!(
        p.edge(&thread, EdgeKind::DecidedBy, &bob),
        "closer is Bob Jones"
    );
    let bob = p.engine.store().node_by_key(&bob).unwrap().unwrap();
    assert!(
        bob.sources.iter().any(|s| s.as_str() == "org")
            && bob.sources.iter().any(|s| s.as_str() == "wiki"),
        "one person node, asserted by the org model and the wiki: {:?}",
        bob.sources
    );
    assert_eq!(bob.trust, TrustLevel::SourceOfTruth);
}

#[test]
fn codeowners_teams_merge_with_the_org_model_team() {
    let p = ingest_pipeline();
    let store = p.engine.store();
    let teams = store
        .nodes_by_kind(anchor::base::NodeKind::Team, 100, 0)
        .unwrap();
    let platform: Vec<_> = teams
        .iter()
        .filter(|t| t.name == "Payments Platform")
        .collect();
    assert_eq!(
        platform.len(),
        1,
        "one node for the org team and @acme/payments-platform"
    );
    let platform = platform[0];
    assert!(
        platform.sources.iter().any(|s| s.as_str() == "org")
            && platform
                .sources
                .iter()
                .any(|s| s.as_str() == "payments-api"),
        "{:?}",
        platform.sources
    );
    assert_eq!(platform.trust, TrustLevel::SourceOfTruth);
    // The same node owns the component (org model) and the files (CODEOWNERS).
    let owned: Vec<_> = store
        .edges_of(platform.id, 100_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Owns && e.from == platform.id)
        .map(|e| store.node(e.to).unwrap().unwrap())
        .collect();
    assert!(owned.iter().any(|n| n.key == keys::component("Ledger")));
    assert!(
        owned
            .iter()
            .any(|n| n.key.as_str().contains("payments-api")
                && n.kind == anchor::base::NodeKind::File),
        "{:?}",
        owned.iter().map(|n| n.key.to_string()).collect::<Vec<_>>()
    );
    // A team only CODEOWNERS knows about stays a separate, still-resolved team.
    assert!(teams.iter().any(|t| t.key.as_str().contains("ledger-crew")));
    // Path-specific rule: /src/ is owned by the person, the rest by the crew.
    let lib = anchor::base::keys::file(&anchor::base::SourceId::new("ledger-writer"), "src/lib.rs");
    let lib_id = p.id(&lib);
    let owners: Vec<String> = store
        .edges_of(lib_id, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Owns && e.to == lib_id)
        .map(|e| store.node(e.from).unwrap().unwrap().key.to_string())
        .collect();
    assert_eq!(
        owners,
        [keys::person("bjones").to_string()],
        "last matching rule wins"
    );
}

#[test]
fn counts_by_kind_are_exact_and_stable() {
    let p = ingest_pipeline();
    let stats = p.engine.store().stats().unwrap();
    let count = |kind: &str| stats.nodes.get(kind).copied().unwrap_or(0);
    println!("nodes: {:?}", stats.nodes);
    for (kind, expected) in [
        ("portfolio", 1),
        ("domain", 1),
        ("component", 1),
        ("service", 2),
        ("team", 3),
        ("person", 2),
        ("api_spec", 1),
        ("api_endpoint", 1),
        ("approval", 1),
        ("wiki_page", 2),
        ("thread", 3), // two wiki threads and the Jira comment thread of PAY-2
        ("comment", 4),
        ("ticket", 2),
    ] {
        assert_eq!(count(kind), expected, "{kind}: {:?}", stats.nodes);
    }
    assert_eq!(
        (count("file"), count("symbol")),
        (3, 4),
        "{:?}",
        stats.nodes
    );
    assert_eq!(stats.node_count, stats.nodes.values().sum::<u64>());

    let (_, edges) = p.engine.store().snapshot().unwrap();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for (_, kind, _, _) in &edges {
        *by_kind.entry(kind.clone()).or_default() += 1;
    }
    println!("edges: {by_kind:?}");
    assert_eq!(edges.len() as u64, stats.edge_count);
    assert_eq!(
        by_kind["blocks"], 2,
        "PAY-1 -> PAY-2 and the rejected approval"
    );
    assert_eq!(by_kind["implements"], 1);
    assert_eq!(by_kind["consumes"], 1);
    assert_eq!(by_kind["approves"], 1);
    assert_eq!(by_kind["reply_to"], 1);
    assert_eq!(by_kind["decided_by"], 2, "approval and dismissed thread");
    assert!(by_kind["contains"] >= 12 && by_kind["owns"] >= 6);
    // The only unresolved claims are the speculative "this page documents <its title>" guesses
    // of the two wiki pages; every explicit reference resolved.
    let pending = p.engine.store().pending_edges(10).unwrap();
    assert_eq!(pending.len() as u64, stats.pending_edges);
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert!(
        pending
            .iter()
            .all(|e| e.kind == EdgeKind::Documents && e.unit.as_str().starts_with("wiki/"))
    );
}

#[test]
fn a_rescan_changes_nothing() {
    let p = ingest_pipeline();
    let before_stats = p.engine.store().stats().unwrap();
    let before = p.engine.store().snapshot().unwrap();
    let before_hlc = p.engine.store().max_hlc().unwrap();
    let again = p.engine.scan_all().unwrap();
    // Sources without per-file change tracking re-submit their units, but identical content is
    // a no-op in the store: nothing is upserted, removed, or re-versioned.
    assert!(
        again
            .iter()
            .all(|r| r.units_removed == 0 && r.nodes_upserted == 0 && r.edges_upserted == 0),
        "{again:#?}"
    );
    assert_eq!(
        p.engine.store().max_hlc().unwrap(),
        before_hlc,
        "no unit was re-versioned"
    );
    let after_stats = p.engine.store().stats().unwrap();
    assert_eq!(after_stats.nodes, before_stats.nodes);
    assert_eq!(
        (
            after_stats.edge_count,
            after_stats.unit_count,
            after_stats.chunk_count
        ),
        (
            before_stats.edge_count,
            before_stats.unit_count,
            before_stats.chunk_count
        )
    );
    assert_eq!(p.engine.store().snapshot().unwrap(), before);

    // Re-opening the engine over the same data directory loads the identical graph.
    let reopened = Engine::open(p.engine.config().clone()).unwrap();
    assert_eq!(reopened.store().snapshot().unwrap(), before);
    assert_graph_matches_store(&reopened);
}

fn assert_graph_matches_store(engine: &Engine) {
    let (mut nodes, mut edges) = engine.store().load_graph().unwrap();
    let shared = engine.graph();
    let graph = shared.read();
    let mut live_nodes: Vec<_> = graph.nodes().collect();
    let mut live_edges: Vec<_> = graph.edges().collect();
    nodes.sort_by_key(|n| n.id);
    edges.sort_by_key(|e| e.id);
    live_nodes.sort_by_key(|n| n.id);
    live_edges.sort_by_key(|e| e.id);
    assert_eq!(live_nodes, nodes, "in-memory nodes differ from the store");
    assert_eq!(live_edges, edges, "in-memory edges differ from the store");
    assert_eq!(graph.ignored_edges(), 0);
}

#[test]
fn the_in_memory_graph_equals_the_store_after_ingestion_edits_and_removals() {
    let p = ingest_pipeline();
    assert_graph_matches_store(&p.engine);

    // Edit a file, delete another, add one: the graph follows every delta.
    let root = p._dir.path();
    write(
        root,
        "repos/payments-api/src/lib.rs",
        &(lib_rs("payments-api") + "\npub fn extra() {}\n"),
    );
    std::fs::remove_file(root.join("repos/payments-api/README.md")).unwrap();
    write(
        root,
        "repos/ledger-writer/src/new.rs",
        "pub fn fresh() {}\n",
    );
    write(
        root,
        "jira/more.json",
        r#"{"key": "PAY-3", "fields": {"summary": "Follow-up of payments-api"}}"#,
    );
    let reports = p.engine.scan_all().unwrap();
    assert!(
        reports
            .iter()
            .map(|r| r.units_applied + r.units_removed)
            .sum::<usize>()
            >= 4
    );
    assert_graph_matches_store(&p.engine);
    let stats = p.engine.store().stats().unwrap();
    assert_eq!(stats.nodes["ticket"], 3);
    assert!(
        p.engine
            .store()
            .node_by_key(&keys::file(
                &anchor::base::SourceId::new("payments-api"),
                "README.md"
            ))
            .unwrap()
            .is_none(),
        "the deleted file's node is gone"
    );
    // Applying the same state twice is a no-op for the graph too.
    p.engine.scan_all().unwrap();
    assert_graph_matches_store(&p.engine);
}

// =============================================================================================
// 2. Tiered routing and the token ledger
// =============================================================================================

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

/// A fake model endpoint: counts requests, replays scripted `(prompt, completion)` token counts
/// and can be switched to failing.
#[derive(Clone, Default)]
struct FakeModel {
    hits: Arc<AtomicUsize>,
    script: Arc<Mutex<VecDeque<(u64, u64)>>>,
    fail: Arc<AtomicBool>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl FakeModel {
    fn script(&self, counts: &[(u64, u64)]) {
        self.script.lock().extend(counts.iter().copied());
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn fake_ollama() -> (String, FakeModel) {
    let fake = FakeModel::default();
    let app = Router::new()
        .route(
            "/api/chat",
            post(
                |State(fake): State<FakeModel>, Json(body): Json<Value>| async move {
                    fake.hits.fetch_add(1, Ordering::SeqCst);
                    fake.bodies.lock().push(body);
                    let (prompt, completion) = fake.script.lock().pop_front().unwrap_or((10, 5));
                    Json(json!({
                        "model": "llama3.2:3b",
                        "message": {"role": "assistant", "content": "local answer"},
                        "done_reason": "stop",
                        "prompt_eval_count": prompt,
                        "eval_count": completion
                    }))
                },
            ),
        )
        .route(
            "/api/version",
            get(|| async { Json(json!({"version": "0.0.0-fake"})) }),
        )
        .with_state(fake.clone());
    (serve(app).await, fake)
}

async fn fake_anthropic() -> (String, FakeModel) {
    let fake = FakeModel::default();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(|State(fake): State<FakeModel>, Json(body): Json<Value>| async move {
                fake.hits.fetch_add(1, Ordering::SeqCst);
                fake.bodies.lock().push(body);
                if fake.fail.load(Ordering::SeqCst) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"type": "error", "error": {"type": "api_error", "message": "boom"}})),
                    );
                }
                let (prompt, completion) = fake.script.lock().pop_front().unwrap_or((10, 5));
                (
                    StatusCode::OK,
                    Json(json!({
                        "model": "claude-sonnet-5-5",
                        "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "cloud answer"}],
                        "usage": {"input_tokens": prompt, "output_tokens": completion}
                    })),
                )
            }),
        )
        .with_state(fake.clone());
    (serve(app).await, fake)
}

fn models_config(
    ollama: &str,
    anthropic: &str,
    routing: RoutingConfig,
) -> anchor::engine::router::ModelsConfig {
    anchor::engine::router::ModelsConfig {
        system1: TierConfig {
            provider: ProviderKind::Ollama,
            model: "llama3.2:3b".into(),
            base_url: Some(ollama.to_owned()),
            timeout_secs: 10,
            ..TierConfig::default()
        },
        system2: TierConfig {
            provider: ProviderKind::Anthropic,
            model: "claude-sonnet-5-5".into(),
            base_url: Some(anthropic.to_owned()),
            api_key_env: Some("ANCHOR_TEST_KEY".into()),
            timeout_secs: 10,
            ..TierConfig::default()
        },
        routing,
    }
}

fn test_env(name: &str) -> Option<String> {
    (name == "ANCHOR_TEST_KEY").then(|| "sk-test".to_owned())
}

struct LedgerSpend(Arc<TokenLedger>);

impl SpendTracker for LedgerSpend {
    fn cloud_spend_today_usd(&self) -> f64 {
        self.0.flush().unwrap();
        self.0.cloud_spend_today_usd().unwrap()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    ledger: Arc<TokenLedger>,
    router: Arc<TieredRouter>,
    ollama: FakeModel,
    anthropic: FakeModel,
}

async fn rig(routing: RoutingConfig) -> Rig {
    let (ollama_url, ollama) = fake_ollama().await;
    let (anthropic_url, anthropic) = fake_anthropic().await;
    let dir = tempfile::tempdir().unwrap();
    let db = anchor::base::Db::open(dir.path().join("anchor.db")).unwrap();
    let ledger = Arc::new(TokenLedger::new(db, PriceTable::default()).unwrap());
    let sink: Arc<dyn UsageSink> = ledger.clone();
    let config = models_config(&ollama_url, &anthropic_url, routing);
    let router = TieredRouter::from_config(&config, sink, &test_env)
        .unwrap()
        .with_spend_tracker(Arc::new(LedgerSpend(ledger.clone())))
        .with_retry_policy(RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        });
    Rig {
        _dir: dir,
        ledger,
        router: Arc::new(router),
        ollama,
        anthropic,
    }
}

fn ask(text: &str) -> CompletionRequest {
    CompletionRequest::prompt(CallPurpose::Synthesize, text)
}

// Prices used below (nano-dollars per token): the cloud model claude-sonnet-5-5 costs
// 2_000 in / 10_000 out; local calls are free and valued at the default baseline model
// claude-opus-5-5: 4_000 in / 20_000 out.
const CLOUD_IN: i64 = 2_000;
const CLOUD_OUT: i64 = 10_000;
const BASE_IN: i64 = 4_000;
const BASE_OUT: i64 = 20_000;

#[tokio::test]
async fn toggling_between_local_and_cloud_is_accounted_exactly() {
    let rig = rig(RoutingConfig::default()).await;
    rig.ollama.script(&[(100, 20), (150, 35), (80, 10)]);
    rig.anthropic.script(&[(1000, 200), (500, 100)]);

    // local, cloud, local, cloud, local: toggling back and forth.
    let order = [
        ModelTier::System1,
        ModelTier::System2,
        ModelTier::System1,
        ModelTier::System2,
        ModelTier::System1,
    ];
    for (index, tier) in order.into_iter().enumerate() {
        let routed = rig
            .router
            .complete(tier, ask(&format!("question {index}")))
            .await
            .unwrap();
        assert_eq!(routed.tier, tier);
        assert!(!routed.degraded);
        let expected = if tier == ModelTier::System1 {
            "local answer"
        } else {
            "cloud answer"
        };
        assert_eq!(routed.completion.text, expected);
    }
    assert_eq!((rig.ollama.hits(), rig.anthropic.hits()), (3, 2));

    rig.ledger.flush().unwrap();
    let summary = rig.ledger.summary(TimeRange::default()).unwrap();
    let (local, cloud) = (summary.local, summary.cloud);
    assert_eq!((local.calls, local.failures), (3, 0));
    assert_eq!((cloud.calls, cloud.failures), (2, 0));
    assert_eq!((local.prompt_tokens, local.completion_tokens), (330, 65));
    assert_eq!((cloud.prompt_tokens, cloud.completion_tokens), (1500, 300));
    assert_eq!(summary.total.calls, 5);
    assert_eq!(summary.total.total_tokens(), 330 + 65 + 1500 + 300);
    // Tier split mirrors locality here.
    assert_eq!(summary.system1.calls, 3);
    assert_eq!(summary.system2.calls, 2);

    // Money, to the nano-dollar.
    assert_eq!(local.cost_nanos, 0, "local calls are free");
    assert_eq!(cloud.cost_nanos, 1500 * CLOUD_IN + 300 * CLOUD_OUT);
    assert_eq!(cloud.cost_nanos, 6_000_000);
    assert_eq!(
        local.cloud_equivalent_nanos,
        330 * BASE_IN + 65 * BASE_OUT,
        "local tokens valued at the baseline cloud model"
    );
    assert_eq!(local.cloud_equivalent_nanos, 2_620_000);
    assert_eq!(
        cloud.cloud_equivalent_nanos, cloud.cost_nanos,
        "a cloud call's equivalent is its own cost"
    );
    assert_eq!(
        summary.savings_nanos, 2_620_000,
        "only local calls save money"
    );
    assert_eq!(summary.total.cost_nanos, 6_000_000);

    // Per-call entries carry locality, model and the exact reported tokens.
    let entries = rig.ledger.recent(10).unwrap();
    assert_eq!(entries.len(), 5);
    let cloud_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.locality == Locality::Cloud)
        .collect();
    assert_eq!(cloud_entries.len(), 2);
    assert!(cloud_entries.iter().all(|e| e.provider == "anthropic"
        && e.model == "claude-sonnet-5-5"
        && e.priced
        && !e.usage_estimated));
    assert!(
        entries
            .iter()
            .filter(|e| e.locality == Locality::Local)
            .all(|e| e.provider == "ollama" && e.cost_nanos == 0)
    );
    assert_eq!(rig.ledger.dropped_records(), 0);
}

#[tokio::test]
async fn a_failing_cloud_endpoint_falls_back_to_local_and_the_failure_is_recorded() {
    let rig = rig(RoutingConfig::default()).await;
    rig.anthropic.script(&[(1000, 200)]);
    let first = rig
        .router
        .complete(ModelTier::System2, ask("a"))
        .await
        .unwrap();
    assert_eq!((first.tier, first.degraded), (ModelTier::System2, false));

    rig.anthropic.fail.store(true, Ordering::SeqCst);
    rig.ollama.script(&[(60, 12)]);
    let second = rig
        .router
        .complete(ModelTier::System2, ask("b"))
        .await
        .unwrap();
    assert_eq!(second.tier, ModelTier::System1, "served locally");
    assert!(second.degraded, "and flagged as degraded");
    assert_eq!(second.completion.text, "local answer");
    assert_eq!((rig.anthropic.hits(), rig.ollama.hits()), (2, 1));

    rig.ledger.flush().unwrap();
    let summary = rig.ledger.summary(TimeRange::default()).unwrap();
    assert_eq!(
        (summary.cloud.calls, summary.cloud.failures),
        (2, 1),
        "the failed attempt is a failed cloud call"
    );
    assert_eq!((summary.local.calls, summary.local.failures), (1, 0));
    assert_eq!(
        summary.cloud.cost_nanos,
        1000 * CLOUD_IN + 200 * CLOUD_OUT,
        "failures cost nothing"
    );
    assert_eq!(
        (summary.cloud.prompt_tokens, summary.cloud.completion_tokens),
        (1000, 200)
    );
    assert_eq!(
        (summary.local.prompt_tokens, summary.local.completion_tokens),
        (60, 12)
    );
    assert_eq!(summary.savings_nanos, 60 * BASE_IN + 12 * BASE_OUT);
    let failed = rig
        .ledger
        .recent(10)
        .unwrap()
        .into_iter()
        .find(|e| !e.ok)
        .expect("failure entry");
    assert_eq!(failed.locality, Locality::Cloud);
    assert!(
        failed.error.as_deref().unwrap().contains("500")
            || failed.error.as_deref().unwrap().contains("boom")
    );
}

#[tokio::test]
async fn a_cloud_failure_with_no_local_tier_is_an_error() {
    let (anthropic_url, anthropic) = fake_anthropic().await;
    anthropic.fail.store(true, Ordering::SeqCst);
    let mut config = models_config(
        "http://127.0.0.1:9",
        &anthropic_url,
        RoutingConfig::default(),
    );
    config.system1.provider = ProviderKind::Disabled;
    let sink: Arc<dyn UsageSink> = Arc::new(anchor::base::MemoryUsageSink::new());
    let router = TieredRouter::from_config(&config, sink, &test_env)
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        });
    let error = router
        .complete(ModelTier::System2, ask("x"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ModelError::Http { status: 500, .. }) || error.is_transient(),
        "{error:?}"
    );
}

#[tokio::test]
async fn local_only_configuration_never_calls_the_cloud() {
    let rig = rig(RoutingConfig {
        allow_cloud: false,
        ..RoutingConfig::default()
    })
    .await;
    rig.ollama.script(&[(40, 8), (41, 9), (42, 10)]);
    for index in 0..3 {
        let routed = rig
            .router
            .complete(ModelTier::System2, ask(&format!("q{index}")))
            .await
            .unwrap();
        assert_eq!(routed.tier, ModelTier::System1);
        assert!(routed.degraded);
    }
    assert_eq!(
        rig.anthropic.hits(),
        0,
        "the cloud endpoint was never contacted"
    );
    rig.ledger.flush().unwrap();
    let summary = rig.ledger.summary(TimeRange::default()).unwrap();
    assert_eq!(summary.cloud.calls, 0);
    assert_eq!(summary.local.calls, 3);
    assert_eq!(
        (summary.local.prompt_tokens, summary.local.completion_tokens),
        (123, 27)
    );
    assert_eq!(summary.total.cost_nanos, 0);
    assert_eq!(summary.savings_nanos, 123 * BASE_IN + 27 * BASE_OUT);
}

#[tokio::test]
async fn the_daily_cloud_budget_stops_cloud_calls_and_local_takes_over() {
    // $0.005 = 5_000_000 nano-dollars; each cloud call below costs 3_000_000.
    let rig = rig(RoutingConfig {
        max_cloud_usd_per_day: Some(0.005),
        ..RoutingConfig::default()
    })
    .await;
    rig.anthropic
        .script(&[(1000, 100), (1000, 100), (1000, 100)]); // 2_000_000 + 1_000_000 each
    let mut tiers = Vec::new();
    for index in 0..4 {
        let routed = rig
            .router
            .complete(ModelTier::System2, ask(&format!("q{index}")))
            .await
            .unwrap();
        tiers.push((routed.tier, routed.degraded));
    }
    assert_eq!(
        tiers,
        [
            (ModelTier::System2, false), // spent 0
            (ModelTier::System2, false), // spent 3_000_000 < 5_000_000
            (ModelTier::System1, true),  // spent 6_000_000 >= cap: blocked
            (ModelTier::System1, true),
        ]
    );
    assert_eq!(
        rig.anthropic.hits(),
        2,
        "no third request reached the cloud"
    );
    rig.ledger.flush().unwrap();
    assert_eq!(rig.ledger.cloud_spend_today_nanos().unwrap(), 6_000_000);
}

#[tokio::test]
async fn query_mode_local_only_never_reaches_the_cloud_but_auto_does() {
    // The knowledge base: the ingestion fixture.
    let pipeline = ingest_pipeline();
    let rig = rig(RoutingConfig::default()).await;
    let queries = QueryEngine::new(QueryDeps {
        store: pipeline.engine.store().clone(),
        graph: pipeline.engine.graph(),
        vectors: None,
        embedder: None,
        router: rig.router.clone(),
        backlog: None,
        graph_config: pipeline.engine.config().graph.clone(),
        query_config: pipeline.engine.config().query.clone(),
    });
    let question =
        "Why is PAY-2 blocked and what does PAY-1 have to do with the payments-api retries?";

    let mut request = QueryRequest::new(question);
    request.mode = QueryMode::LocalOnly;
    let local = queries.query(request).await.unwrap();
    assert_eq!(rig.anthropic.hits(), 0, "local-only never calls the cloud");
    assert!(
        local.usage.iter().all(|u| u.tier == ModelTier::System1),
        "{:?}",
        local.usage
    );

    let auto = queries.query(QueryRequest::new(question)).await.unwrap();
    let escalated = auto.usage.iter().any(|u| u.tier == ModelTier::System2);
    println!("auto route: {:?}, usage {:?}", auto.route, auto.usage);
    assert_eq!(
        escalated,
        rig.anthropic.hits() > 0,
        "the cloud is contacted exactly when the query escalated"
    );
    assert!(
        escalated,
        "a complex 'why' question escalates to System 2: {:?}",
        auto.route
    );
    rig.ledger.flush().unwrap();
    let summary = rig.ledger.summary(TimeRange::default()).unwrap();
    assert_eq!(summary.cloud.calls as usize, rig.anthropic.hits());
    assert!(summary.local.calls >= 1);
}
