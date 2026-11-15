//! Zoom levels: sources ingested shallowly are flagged in answers and can be deepened on demand.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use anchor::base::{SourceId, SourceKind};
use anchor::config::{AnchorConfig, Depth, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::query::{DepthControl, QueryDeps, QueryEngine, QueryRequest, ShallowPolicy};
use anchor::engine::router::ProviderKind;

fn engine(dir: &std::path::Path, depth: Depth, policy: ShallowPolicy) -> Arc<Engine> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("CODEOWNERS"), "* @acme/payments-platform\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// Charges a card.\npub fn charge(amount: u64) -> u64 { validate(amount) }\nfn validate(amount: u64) -> u64 { amount }\n",
    )
    .unwrap();
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.query.on_shallow = policy;
    let mut source = SourceConfig::new("payments-api", SourceKind::Repo, repo.to_string_lossy());
    source.depth = depth;
    config.sources.push(source);
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    engine
}

fn queries(engine: &Arc<Engine>) -> QueryEngine {
    let depths: Arc<dyn DepthControl> = engine.clone();
    QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: None,
        embedder: None,
        router: engine.router(),
        backlog: Some(engine.backlog()),
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    })
    .with_depths(depths)
}

#[test]
fn each_depth_ingests_more_than_the_one_above() {
    let counts: Vec<(u64, u64)> = [Depth::PhoneBook, Depth::Map, Depth::Full]
        .into_iter()
        .map(|depth| {
            let dir = tempfile::tempdir().unwrap();
            let stats = engine(dir.path(), depth, ShallowPolicy::Manual)
                .store()
                .stats()
                .unwrap();
            (stats.node_count, stats.edge_count)
        })
        .collect();
    // Phone book: the service and its owner. Map: plus files and symbols. Full: plus calls.
    assert_eq!(counts[0], (2, 1));
    assert!(counts[1].0 > counts[0].0);
    assert_eq!(counts[2].0, counts[1].0);
    assert!(counts[2].1 > counts[1].1, "{counts:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_policy_answers_shallow_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), Depth::PhoneBook, ShallowPolicy::Manual);
    let queries = queries(&engine);

    let owner = queries
        .query(QueryRequest::new("Who owns the payments-api service?"))
        .await
        .unwrap();
    assert!(owner.answer.unwrap().contains("payments-platform"));
    assert_eq!(owner.shallow.len(), 1);
    assert_eq!(owner.shallow[0].status, "manual");
    assert_eq!(owner.shallow[0].depth, Depth::PhoneBook);

    let deep = queries
        .query(QueryRequest::new("Where is charge defined?"))
        .await
        .unwrap();
    assert!(
        deep.context
            .items
            .iter()
            .all(|item| !item.node.key.contains("#charge"))
    );
    assert_eq!(deep.shallow[0].source, "payments-api");
    assert_eq!(
        engine.depth(&SourceId::new("payments-api")),
        Depth::PhoneBook
    );

    engine
        .deepen(&SourceId::new("payments-api"), Depth::Full)
        .unwrap();
    let after = queries
        .query(QueryRequest::new("Where is charge defined?"))
        .await
        .unwrap();
    assert!(
        after
            .context
            .items
            .iter()
            .any(|item| item.node.key.ends_with("#charge"))
    );
    assert!(after.shallow.is_empty(), "{:?}", after.shallow);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_policy_deepens_before_answering() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), Depth::PhoneBook, ShallowPolicy::Block);
    let response = queries(&engine)
        .query(QueryRequest::new("Where is charge defined?"))
        .await
        .unwrap();
    assert!(
        response
            .context
            .items
            .iter()
            .any(|item| item.node.key.ends_with("#charge"))
    );
    assert_eq!(response.shallow[0].status, "deepened");
    assert_eq!(engine.depth(&SourceId::new("payments-api")), Depth::Full);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_policy_answers_now_and_deepens_behind() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), Depth::PhoneBook, ShallowPolicy::Background);
    let response = queries(&engine)
        .query(QueryRequest::new("Where is charge defined?"))
        .await
        .unwrap();
    assert_eq!(response.shallow[0].status, "deepening");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.store().stats().unwrap().node_count < 4 {
        assert!(
            std::time::Instant::now() < deadline,
            "background deepening never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(engine.depth(&SourceId::new("payments-api")), Depth::Full);
}

#[test]
fn depth_survives_a_restart_and_never_gets_shallower() {
    let dir = tempfile::tempdir().unwrap();
    let first = engine(dir.path(), Depth::PhoneBook, ShallowPolicy::Manual);
    first
        .deepen(&SourceId::new("payments-api"), Depth::Map)
        .unwrap();
    drop(first);
    let again = engine(dir.path(), Depth::PhoneBook, ShallowPolicy::Manual);
    assert_eq!(again.depth(&SourceId::new("payments-api")), Depth::Map);
    again
        .deepen(&SourceId::new("payments-api"), Depth::PhoneBook)
        .unwrap();
    assert_eq!(again.depth(&SourceId::new("payments-api")), Depth::Map);
}
