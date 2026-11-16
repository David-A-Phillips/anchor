//! End-to-end performance and resource-footprint assertions.
//!
//! One test function runs the scenarios sequentially so that measurements of one scenario are
//! not disturbed by another:
//!
//! * file change detected → graph mutated: p95 under 50 ms;
//! * CPU while idle (watcher running, nothing changing): ~0 %;
//! * resident memory under a heavy concurrent query load: under 3 GB;
//! * SQLite and `.contextpack` footprint measured at two scales and projected to 1,000 repos.
// Reads CPU time and resident memory through Unix interfaces.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anchor::base::{SourceKind, TrustLevel};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use anchor::engine::router::ProviderKind;
use anchor::engine::watcher::FileWatcher;
use anchor::engine::{Engine, EngineEvent};

const FILES_PER_REPO: usize = 40;

fn write_repo(root: &Path, name: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("CODEOWNERS"), format!("* @acme/{name}-team\n")).unwrap();
    std::fs::write(
        dir.join("README.md"),
        format!("# {name}\n\nThe {name} service.\n"),
    )
    .unwrap();
    for index in 0..FILES_PER_REPO {
        std::fs::write(
            dir.join(format!("src/module_{index}.rs")),
            source_file(name, index, 0),
        )
        .unwrap();
    }
}

fn source_file(repo: &str, index: usize, revision: usize) -> String {
    let mut text = format!("//! Module {index} of {repo}.\n\n");
    for function in 0..12 {
        text.push_str(&format!(
            "/// Handles step {function} (revision {revision}).\npub fn step_{index}_{function}(input: u64) -> u64 {{\n    step_{index}_{}(input + {revision})\n}}\n\n",
            (function + 1) % 12
        ));
    }
    text
}

fn config(root: &Path, repos: &[String]) -> AnchorConfig {
    let mut config = AnchorConfig {
        data_dir: Some(root.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    for repo in repos {
        let mut source = SourceConfig::new(
            repo.as_str(),
            SourceKind::Repo,
            root.join(repo).to_string_lossy(),
        );
        source.trust = TrustLevel::SourceOfTruth;
        config.sources.push(source);
    }
    config
}

fn cpu_time() -> Duration {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).unwrap();
    let micros = |tv: nix::sys::time::TimeVal| tv.tv_sec() as u64 * 1_000_000 + tv.tv_usec() as u64;
    Duration::from_micros(micros(usage.user_time()) + micros(usage.system_time()))
}

fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

fn query_engine(engine: &Engine) -> QueryEngine {
    QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: Some(engine.vectors()),
        embedder: Some(engine.embedder()),
        router: engine.router(),
        backlog: Some(engine.backlog()),
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    })
}

fn footprint(repos: usize) -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..repos).map(|i| format!("svc-{i}")).collect();
    for name in &names {
        write_repo(dir.path(), name);
    }
    let engine = Engine::open(config(dir.path(), &names)).unwrap();
    engine.scan_all().unwrap();
    engine.store().db().checkpoint().unwrap();
    let pack = engine.export_pack(None).unwrap();
    (
        engine.store().db().size_on_disk().unwrap(),
        std::fs::metadata(pack).unwrap().len(),
    )
}

#[test]
fn performance_and_footprint_budgets() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..4).map(|i| format!("svc-{i}")).collect();
    for name in &names {
        write_repo(dir.path(), name);
    }
    let engine = Engine::open(config(dir.path(), &names)).unwrap();
    let started = Instant::now();
    let reports = engine.scan_all().unwrap();
    println!(
        "initial scan: {} files in {:?}",
        reports.iter().map(|r| r.files_seen).sum::<usize>(),
        started.elapsed()
    );
    let stats = engine.store().stats().unwrap();
    assert!(stats.node_count > 1500, "{stats:?}");

    // --- 1. file change -> graph mutation latency -------------------------------------------
    let mut watcher = FileWatcher::start(engine.clone()).unwrap();
    let mut events = engine.subscribe();
    std::thread::sleep(Duration::from_millis(200));
    let edits = 60;
    for revision in 1..=edits {
        let repo = &names[revision % names.len()];
        let index = revision % FILES_PER_REPO;
        let path = dir.path().join(repo).join(format!("src/module_{index}.rs"));
        std::fs::write(&path, source_file(repo, index, revision)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match events.try_recv() {
                Ok(EngineEvent::FileIngested { .. }) => break,
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "edit {revision} was never ingested"
                    );
                    std::thread::sleep(Duration::from_micros(500));
                }
            }
        }
    }
    let latency = engine.latency_stats();
    println!(
        "change -> graph latency over {} edits: p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        latency.count,
        latency.p50_us as f64 / 1e3,
        latency.p95_us as f64 / 1e3,
        latency.max_us as f64 / 1e3
    );
    assert!(latency.count >= edits);
    assert!(
        latency.p95_us < 50_000,
        "p95 latency {} us exceeds the 50 ms budget",
        latency.p95_us
    );

    // --- 2. idle CPU --------------------------------------------------------------------------
    std::thread::sleep(Duration::from_millis(500));
    let (cpu_before, wall) = (cpu_time(), Instant::now());
    std::thread::sleep(Duration::from_secs(3));
    let cpu = cpu_time() - cpu_before;
    let percent = cpu.as_secs_f64() / wall.elapsed().as_secs_f64() * 100.0;
    println!("idle CPU: {percent:.3} % of one core over 3 s");
    assert!(percent < 1.0, "idle CPU {percent:.2} % is not ~0 %");
    watcher.stop();

    // --- 3. memory under heavy query load ------------------------------------------------------
    let queries = query_engine(&engine);
    let peak = runtime.block_on(async {
        let mut tasks = Vec::new();
        for worker in 0..16usize {
            let queries = queries.clone();
            tasks.push(tokio::spawn(async move {
                let mut peak = 0;
                for round in 0..125usize {
                    let question = match round % 3 {
                        0 => format!("Who owns the svc-{} service?", worker % 4),
                        1 => format!(
                            "Where is step_{}_{} defined?",
                            round % FILES_PER_REPO,
                            round % 12
                        ),
                        _ => format!("What depends on step_{worker}_3?"),
                    };
                    let mut request = QueryRequest::new(question);
                    request.mode = QueryMode::RetrievalOnly;
                    let response = queries.query(request).await.unwrap();
                    assert!(!response.context.items.is_empty());
                    peak = peak.max(resident_bytes());
                }
                peak
            }));
        }
        let mut peak = 0;
        for task in tasks {
            peak = peak.max(task.await.unwrap());
        }
        peak
    });
    println!(
        "peak resident memory under 2,000 concurrent queries: {:.0} MB",
        peak as f64 / 1e6
    );
    assert!(
        peak < 3 * 1024 * 1024 * 1024,
        "peak RSS {peak} exceeds 3 GB"
    );
    let answer = runtime
        .block_on(queries.query(QueryRequest::new("Who owns the svc-1 service?")))
        .unwrap();
    assert!(answer.answer.unwrap_or_default().contains("svc-1-team"));
    drop(engine);

    // --- 4. footprint projection ---------------------------------------------------------------
    let (db_small, pack_small) = footprint(4);
    let (db_large, pack_large) = footprint(12);
    let per_repo_db = (db_large - db_small) as f64 / 8.0;
    let per_repo_pack = (pack_large - pack_small) as f64 / 8.0;
    println!(
        "footprint: {FILES_PER_REPO} files/repo -> SQLite {:.0} KB/repo, .contextpack {:.0} KB/repo; projected 1,000 repos: SQLite {:.2} GB, .contextpack {:.0} MB",
        per_repo_db / 1e3,
        per_repo_pack / 1e3,
        per_repo_db * 1000.0 / 1e9,
        per_repo_pack * 1000.0 / 1e6
    );
    assert!(
        per_repo_pack > 0.0 && per_repo_db > per_repo_pack,
        "the pack must be smaller than the database"
    );
    assert!(
        per_repo_pack * 1000.0 < 1e9,
        "projected .contextpack for 1,000 repos exceeds 1 GB"
    );
    assert!(
        per_repo_db * 1000.0 < 20e9,
        "projected SQLite footprint for 1,000 repos exceeds 20 GB"
    );
    let _ = Arc::new(());
}
