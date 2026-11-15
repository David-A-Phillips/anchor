//! Criterion benchmarks of the engine's hot paths:
//!
//! * parsing a source file from scratch and incrementally after a small edit;
//! * `Pathfinder::nearest` (trust-weighted ownership search) on a generated graph;
//! * a retrieval-only query on the synthetic enterprise (small preset);
//! * `.contextpack` export of that enterprise.
#![allow(clippy::unwrap_used)]

use std::hint::black_box;

use anchor::base::{EdgeId, EdgeKind, GraphEdge, GraphNode, NodeId, NodeKind, TrustLevel};
use anchor::config::AnchorConfig;
use anchor::engine::Engine;
use anchor::engine::graph::{
    ContextGraph, Goal, GraphConfig, Limits, Pathfinder, Seed, TraversalProfile,
};
use anchor::engine::parser::{IncrementalParser, Language, ParserConfig, parse_source};
use anchor::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use anchor::synth::{self, Preset, SynthSpec};
use criterion::{Criterion, criterion_group, criterion_main};

// --- parsing ---------------------------------------------------------------------------------

fn rust_source(functions: usize, revision: usize) -> String {
    let mut text = String::from("//! Benchmark module.\n\nuse std::collections::HashMap;\n\n");
    for index in 0..functions {
        text.push_str(&format!(
            "/// Computes step {index}.\npub fn step_{index}(input: u64, table: &HashMap<u64, u64>) -> u64 {{\n    let base = table.get(&input).copied().unwrap_or({});\n    step_{}(base + input)\n}}\n\n",
            if index == functions / 2 { revision } else { 0 },
            (index + 1) % functions
        ));
    }
    text
}

fn parsing(c: &mut Criterion) {
    let text = rust_source(200, 0);
    let mut group = c.benchmark_group("parse");
    group.throughput(criterion::Throughput::Bytes(text.len() as u64));
    group.bench_function("full_200_fn_rust_file", |b| {
        b.iter(|| parse_source(Language::Rust, black_box(&text)).unwrap());
    });

    let parser = IncrementalParser::new(&ParserConfig::default());
    parser.parse("bench.rs", Language::Rust, &text).unwrap();
    let mut revision = 0;
    group.bench_function("incremental_one_line_edit", |b| {
        b.iter(|| {
            revision += 1;
            let edited = rust_source(200, revision % 1000);
            parser
                .parse("bench.rs", Language::Rust, black_box(&edited))
                .unwrap()
        });
    });
    group.bench_function("incremental_text_generation_only", |b| {
        b.iter(|| {
            revision += 1;
            black_box(rust_source(200, revision % 1000))
        });
    });
    group.finish();
}

// --- pathfinding -----------------------------------------------------------------------------

/// A hierarchy of `size` nodes (fan-out 6) with teams owning a sample of the nodes, about one in
/// seven ownership claims being a stale reference-only one.
fn generated_graph(size: i64) -> (ContextGraph, Vec<NodeId>) {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let teams = 200;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for id in 0..size {
        let kind = match id {
            0 => NodeKind::Portfolio,
            1..=6 => NodeKind::Domain,
            7..=42 => NodeKind::Component,
            _ => NodeKind::Service,
        };
        nodes.push(GraphNode {
            id: NodeId(id),
            kind,
            trust: TrustLevel::SourceOfTruth,
        });
        if id > 0 {
            edges.push(GraphEdge {
                id: EdgeId(edges.len() as i64),
                from: NodeId((id - 1) / 6),
                to: NodeId(id),
                kind: EdgeKind::Contains,
                trust: TrustLevel::SourceOfTruth,
                weight: 1.0,
            });
        }
    }
    for team in 0..teams {
        let id = size + team;
        nodes.push(GraphNode {
            id: NodeId(id),
            kind: NodeKind::Team,
            trust: TrustLevel::SourceOfTruth,
        });
        for _ in 0..(size / teams / 4).max(1) {
            let target = (next() % size as u64) as i64;
            let stale = next() % 7 == 0;
            edges.push(GraphEdge {
                id: EdgeId(edges.len() as i64),
                from: NodeId(id),
                to: NodeId(target),
                kind: EdgeKind::Owns,
                trust: if stale {
                    TrustLevel::ReferenceOnly
                } else {
                    TrustLevel::SourceOfTruth
                },
                weight: 1.0,
            });
        }
    }
    let services: Vec<NodeId> = (43..size).step_by(97).map(NodeId).collect();
    (ContextGraph::from_parts(nodes, edges), services)
}

fn pathfinding(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathfinder_nearest_team");
    let config = GraphConfig::default();
    let profile = TraversalProfile::ownership();
    let goal = Goal::kinds(&[NodeKind::Team]);
    for size in [10_000i64, 100_000] {
        let (graph, services) = generated_graph(size);
        let finder = Pathfinder::new(&graph, &config);
        let limits = Limits::from(&config);
        let mut cursor = 0;
        group.bench_function(format!("{size}_nodes"), |b| {
            b.iter(|| {
                cursor = (cursor + 1) % services.len();
                finder.nearest(
                    &[Seed::new(services[cursor])],
                    black_box(&goal),
                    3,
                    &profile,
                    limits,
                )
            });
        });
    }
    group.finish();
}

// --- retrieval and export on the synthetic enterprise ----------------------------------------

struct Enterprise {
    _dir: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
    engine: std::sync::Arc<Engine>,
    queries: QueryEngine,
    questions: Vec<String>,
}

fn enterprise() -> Enterprise {
    let dir = tempfile::tempdir().unwrap();
    synth::generate(&SynthSpec::preset(Preset::Small).seed(7), dir.path()).unwrap();
    let config = AnchorConfig::from_toml_str(
        &std::fs::read_to_string(dir.path().join("anchor.toml")).unwrap(),
    )
    .unwrap();
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let truth: synth::GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();
    let questions = truth
        .ownership
        .iter()
        .map(|q| q.question.clone())
        .chain(truth.justification.iter().map(|q| q.question.clone()))
        .collect();
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
    Enterprise {
        _dir: dir,
        runtime: tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap(),
        engine,
        queries,
        questions,
    }
}

fn retrieval_and_export(c: &mut Criterion) {
    let world = enterprise();
    let mut group = c.benchmark_group("enterprise_small");
    let mut cursor = 0;
    group.bench_function("retrieval_only_query", |b| {
        b.iter(|| {
            cursor = (cursor + 1) % world.questions.len();
            let mut request = QueryRequest::new(world.questions[cursor].as_str());
            request.mode = QueryMode::RetrievalOnly;
            world
                .runtime
                .block_on(world.queries.query(black_box(request)))
                .unwrap()
        });
    });
    group.sample_size(20);
    group.bench_function("contextpack_export", |b| {
        b.iter(|| {
            let path = world.engine.export_pack(None).unwrap();
            // Packs accumulate in the data directory; drop each one so the loop stays flat.
            let size = std::fs::metadata(&path).unwrap().len();
            std::fs::remove_file(&path).unwrap();
            size
        });
    });
    group.finish();
}

criterion_group!(benches, parsing, pathfinding, retrieval_and_export);
criterion_main!(benches);
