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
        "/// A ledger.\npub struct Ledger;\n/// Charges a card.\npub fn charge(amount: u64) -> u64 { validate(amount) }\nfn validate(amount: u64) -> u64 { amount }\n",
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
    // Phone book: the service and its owner. Map: plus files and public types. Full: plus
    // functions, private items and calls.
    assert_eq!(counts[0], (2, 1));
    assert!(counts[1].0 > counts[0].0, "{counts:?}");
    assert!(counts[2].0 > counts[1].0, "{counts:?}");
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
    while engine.store().stats().unwrap().node_count < 5 {
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

/// Measures what each depth costs and what it can answer, on the synthetic enterprise with known
/// answers. Run with `--nocapture` to see the table.
#[tokio::test(flavor = "multi_thread")]
async fn depth_trades_footprint_for_recall() {
    use anchor::engine::query::QueryMode;
    use anchor::synth::{GroundTruth, Preset, SynthSpec, generate};

    let data = tempfile::tempdir().unwrap();
    generate(&SynthSpec::preset(Preset::Small).seed(2024), data.path()).unwrap();
    let truth: GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(data.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();

    let mut rows = Vec::new();
    for depth in [Depth::PhoneBook, Depth::Map, Depth::Full] {
        let home = tempfile::tempdir().unwrap();
        let mut config = AnchorConfig::load(&data.path().join("anchor.toml")).unwrap();
        config.data_dir = Some(home.path().to_path_buf());
        config.query.on_shallow = ShallowPolicy::Manual;
        for source in &mut config.sources {
            source.depth = depth;
        }
        let engine = Engine::open(config).unwrap();
        engine.scan_all().unwrap();
        engine.store().db().checkpoint().unwrap();
        let stats = engine.store().stats().unwrap();
        let graph_bytes = engine.graph().read().memory_estimate_bytes();
        let queries = queries(&engine);

        let mut recall = Vec::new();
        let mut flagged = 0usize;
        let mut asked = 0usize;
        let groups: [(&str, Vec<(String, Vec<String>)>); 3] = [
            (
                "ownership",
                truth
                    .ownership
                    .iter()
                    .map(|q| (q.question.clone(), q.expected_owner_keys.clone()))
                    .collect(),
            ),
            (
                "justification",
                truth
                    .justification
                    .iter()
                    .map(|q| (q.question.clone(), q.expected_keys.clone()))
                    .collect(),
            ),
            (
                "relationships",
                truth
                    .relationships
                    .iter()
                    .map(|q| (q.question.clone(), q.expected_keys.clone()))
                    .collect(),
            ),
        ];
        for (name, questions) in groups {
            let (mut found, mut expected) = (0usize, 0usize);
            for (question, keys) in questions {
                let mut request = QueryRequest::new(question);
                request.mode = QueryMode::RetrievalOnly;
                let response = queries.query(request).await.unwrap();
                asked += 1;
                flagged += usize::from(!response.shallow.is_empty());
                expected += keys.len();
                found += keys
                    .iter()
                    .filter(|key| {
                        response
                            .context
                            .items
                            .iter()
                            .any(|item| &&item.node.key == key)
                    })
                    .count();
            }
            recall.push((name, found as f64 / expected.max(1) as f64));
        }
        println!(
            "{:<10} db {:>6.1} MB  graph {:>5.0} KB  {:>6} nodes {:>6} edges {:>6} chunks | recall own {:.2} just {:.2} rel {:.2} | flagged shallow {}/{}",
            depth.as_str(),
            stats.db_bytes as f64 / 1e6,
            graph_bytes as f64 / 1e3,
            stats.node_count,
            stats.edge_count,
            stats.chunk_count,
            recall[0].1,
            recall[1].1,
            recall[2].1,
            flagged,
            asked
        );
        rows.push((stats.db_bytes, recall[0].1, flagged, asked));
    }
    // Deeper is never smaller, and full depth is never flagged shallow.
    assert!(rows[0].0 <= rows[1].0 && rows[1].0 <= rows[2].0, "{rows:?}");
    assert_eq!(rows[2].2, 0);
    // Every answer from a shallow graph says so.
    assert_eq!(rows[0].2, rows[0].3);
    // Ownership is a phone-book question: it must already work at the top level.
    assert!(
        rows[0].1 > 0.5,
        "ownership recall at phone-book depth: {}",
        rows[0].1
    );
}
