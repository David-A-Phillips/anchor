//! Opening a database written by an earlier release.
//!
//! `fixtures/store-0.1.1-beta-tiny-77.db.gz` is the database `anchor 0.1.1-beta` wrote after
//! `anchor demo --scale tiny --seed 77` and `anchor ingest` (checkpointed and vacuumed). It
//! predates graph versions and the delta log (store migration 3) and the saved graph. Opening it
//! must migrate it in place, build the graph from its tables, and leave it working exactly like
//! a database this version created: same graph, same answers, and incremental from then on.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use anchor::config::AnchorConfig;
use anchor::engine::Engine;
use anchor::engine::impact::{HotspotOptions, ImpactOptions};
use anchor::synth::{self, Preset, RelationshipKind, SynthSpec};

const FIXTURE: &str = "tests/fixtures/store-0.1.1-beta-tiny-77.db.gz";
/// Graph size the 0.1.1-beta database holds (`SELECT count(*) FROM nodes / edges`).
const OLD_NODES: usize = 361;
const OLD_EDGES: usize = 847;

/// The demo corpus the fixture was ingested from, with the old database as its data.
fn upgraded_world(dir: &Path) -> AnchorConfig {
    synth::generate(&SynthSpec::preset(Preset::Tiny).seed(77), dir).unwrap();
    let config =
        AnchorConfig::from_toml_str(&std::fs::read_to_string(dir.join("anchor.toml")).unwrap())
            .unwrap();
    let data = config.paths().root().to_path_buf();
    std::fs::create_dir_all(&data).unwrap();
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(FIXTURE).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    std::fs::write(config.paths().db(), bytes).unwrap();
    config
}

fn migrations(config: &AnchorConfig) -> BTreeSet<(String, i64)> {
    let conn = rusqlite::Connection::open(config.paths().db()).unwrap();
    let mut statement = conn
        .prepare("SELECT module, version FROM anchor_migrations")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_0_1_1_database_is_migrated_and_works_like_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = upgraded_world(dir.path());
    let before = migrations(&config);
    assert!(before.contains(&("store".to_owned(), 2)));
    assert!(
        !before.contains(&("store".to_owned(), 3)),
        "fixture is pre-migration"
    );

    // First open: migrated, and the graph is built from the tables (no snapshot yet).
    let engine = Engine::open(config.clone()).unwrap();
    assert!(migrations(&config).contains(&("store".to_owned(), 3)));
    let open = engine.graph_open();
    assert!(!open.from_snapshot, "{open:?}");
    {
        let graph = engine.graph();
        let graph = graph.read();
        assert_eq!(graph.node_count(), OLD_NODES);
        assert_eq!(graph.edge_count(), OLD_EDGES);
    }
    let tables = engine.store().snapshot().unwrap();

    // Answers come out of the old data: every consumer of an API is found.
    let truth: synth::GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();
    let mut checked = 0;
    for question in truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
    {
        let report = engine
            .impact(
                std::slice::from_ref(&question.subject_key),
                &ImpactOptions::default(),
            )
            .unwrap();
        let found: BTreeSet<&str> = report
            .dependents
            .iter()
            .map(|d| d.entity.key.as_str())
            .collect();
        for consumer in &question.expected_keys {
            assert!(found.contains(consumer.as_str()), "{consumer}");
            checked += 1;
        }
    }
    assert!(checked > 0);
    assert!(
        !engine
            .hotspots(&HotspotOptions::default())
            .unwrap()
            .hotspots
            .is_empty()
    );

    // Re-ingesting the same corpus leaves the graph as it was, and the in-memory graph still
    // mirrors the tables.
    engine.scan_all().unwrap();
    let after = engine.store().snapshot().unwrap();
    assert_eq!(after.0.len(), tables.0.len());
    assert_eq!(after.1.len(), tables.1.len());
    {
        let graph = engine.graph();
        let graph = graph.read();
        assert_eq!(graph.node_count(), OLD_NODES);
        assert_eq!(graph.edge_count(), OLD_EDGES);
    }
    engine.flush().unwrap();
    drop(engine);

    // Second open: from the snapshot the first run saved, with the same graph.
    let engine = Engine::open(config).unwrap();
    assert!(
        engine.graph_open().from_snapshot,
        "{:?}",
        engine.graph_open()
    );
    let graph = engine.graph();
    let graph = graph.read();
    assert_eq!(graph.node_count(), OLD_NODES);
    assert_eq!(graph.edge_count(), OLD_EDGES);
}

#[test]
fn a_0_1_1_database_takes_new_writes_incrementally() {
    let dir = tempfile::tempdir().unwrap();
    let config = upgraded_world(dir.path());
    let engine = Engine::open(config.clone()).unwrap();
    let (_, start) = engine.store().graph_version().unwrap();

    // Change one source file: one unit is applied and logged as one graph version.
    let repo = std::fs::read_dir(dir.path().join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(
        repo.join("NEW_NOTES.md"),
        "# Notes\n\nAdded after the upgrade.\n",
    )
    .unwrap();
    let reports = engine.scan_all().unwrap();
    let applied: usize = reports.iter().map(|r| r.units_applied).sum();
    assert!(applied >= 1, "{reports:#?}");
    let (_, now) = engine.store().graph_version().unwrap();
    assert!(now > start);
    assert!(engine.store().graph_log_len().unwrap() >= 1);
    let nodes = engine.graph().read().node_count();
    assert!(nodes > OLD_NODES);
    engine.flush().unwrap();
    drop(engine);

    // Reopened, the new state is there.
    let engine = Engine::open(config).unwrap();
    assert_eq!(engine.graph().read().node_count(), nodes);
}
