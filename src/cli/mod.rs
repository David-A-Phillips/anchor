//! Command-line interface (`anchor <command>`).

pub mod eval;
pub mod fetch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::base::{Error, Result, SourceId};
use crate::config::provision::{
    OllamaManager, ProvisionEvent, ProvisionOptions, RebuildOptions, TeardownOptions,
};
use crate::config::wizard;
use crate::config::{AnchorConfig, default_config_path};
use crate::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use crate::engine::watcher::FileWatcher;
use crate::engine::{Engine, EngineEvent};
use crate::metrics::{TimeRange, format_usd};
use crate::sync::identity::{DeviceIdentity, TrustPolicy, TrustedDevices};
use crate::sync::{
    ClientOptions, GraphUnitStore, ServerOptions, SyncClient, SyncServer, SyncTiles, token_from_env,
};

/// Anchor: a local-first context engine that grounds AI models in your code, docs and org.
#[derive(Debug, Parser)]
#[command(name = "anchor", version, about)]
struct Cli {
    /// Configuration file (default: $ANCHOR_CONFIG or the platform config directory).
    #[arg(long, global = true, env = "ANCHOR_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Interactive setup wizard: Ollama, model tiers, sources and trust levels, embeddings.
    Setup {
        /// Accept every default and ask nothing (installs Ollama if none is reachable).
        #[arg(long)]
        yes: bool,
        /// Also run the bootstrap resilience test: wipe, remove and rebuild the managed Ollama.
        #[arg(long)]
        rebuild_test: bool,
    },
    /// Write a starter configuration file.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Scan configured sources into the graph (all, or one with --source).
    Ingest {
        /// Only this source id.
        #[arg(long)]
        source: Option<String>,
    },
    /// Ask a question.
    Query {
        /// The question.
        text: String,
        /// Print the full JSON response.
        #[arg(long)]
        json: bool,
        /// Never call the cloud tier.
        #[arg(long)]
        local_only: bool,
        /// Retrieval only: no model call.
        #[arg(long)]
        retrieval_only: bool,
        /// Ask a model to write a prose answer from the retrieved evidence (slower).
        #[arg(long)]
        explain: bool,
    },
    /// Run the daemon: initial scan, file watching and the dashboard.
    Daemon {
        /// Do not serve the dashboard.
        #[arg(long)]
        no_web: bool,
    },
    /// Show graph, source and spend statistics.
    Status,
    /// Show token spend (local vs. cloud) for the last N days.
    Spend {
        /// Days to cover.
        #[arg(long, default_value_t = 30)]
        days: u64,
    },
    /// List open gap-analysis backlog items.
    Backlog,
    /// Let the local model summarise and tag pages, tickets, pull requests and APIs.
    Enrich {
        /// Maximum nodes to process in this run.
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Score the engine against questions with known answers (see `anchor demo`).
    Eval {
        /// The ground-truth file (`ground_truth.json`).
        #[arg(long)]
        truth: PathBuf,
        /// Never call the cloud tier.
        #[arg(long)]
        local_only: bool,
        /// Retrieval only: no model call for answers.
        #[arg(long)]
        retrieval_only: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export a GitHub repository's issues, discussions and pull-request reviews (needs `gh`).
    Fetch {
        /// `owner/name` of the repository; omit to refresh every `[[github]]` entry.
        repository: Option<String>,
        /// Output directory (default: <data dir>/github/<owner>-<name>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Most recently updated issues to export.
        #[arg(long, default_value_t = 100)]
        issues: usize,
        /// Most recently updated discussions to export.
        #[arg(long, default_value_t = 50)]
        discussions: usize,
        /// Most recently updated pull requests to export.
        #[arg(long, default_value_t = 50)]
        pulls: usize,
    },
    /// Ingest a source more deeply (zoom in): phone_book -> map -> full.
    Deepen {
        /// Source id.
        source: String,
        /// Target depth.
        #[arg(long, default_value = "full")]
        depth: crate::config::Depth,
    },
    /// Export a signed .contextpack snapshot of the graph.
    Export {
        /// Copy the pack to this path (default: the data directory's packs folder).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Verify and merge a .contextpack.
    Import {
        /// The pack file.
        file: PathBuf,
    },
    /// Run the central sync server (WebSocket and gRPC on `sync.listen`).
    Server,
    /// Synchronise once with the configured sync server and print the result.
    Sync,
    /// Generate a synthetic enterprise (repos, org, wiki, portal, Jira, reviews) to try Anchor
    /// without connecting real systems.
    Demo {
        /// Directory to write the dataset into.
        #[arg(long)]
        out: PathBuf,
        /// Size of the dataset.
        #[arg(long, value_enum, default_value_t = DemoScale::Small)]
        scale: DemoScale,
        /// Seed: the same seed always yields the same dataset.
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Manage the local Ollama backend.
    Ollama {
        #[command(subcommand)]
        action: OllamaAction,
    },
}

/// Sizes of the `demo` dataset.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum DemoScale {
    /// 8 repositories.
    Tiny,
    /// 24 repositories.
    Small,
    /// 100 repositories.
    Medium,
}

#[derive(Debug, Subcommand)]
enum OllamaAction {
    /// Show whether Ollama is reachable and whether Anchor manages it.
    Status,
    /// Install (if needed), start, pull the default model and verify.
    Provision {
        /// Model to pull instead of the configured default.
        #[arg(long)]
        model: Option<String>,
    },
    /// Stop and wipe the Anchor-managed Ollama installation and models.
    Teardown {
        /// Keep the installed distribution.
        #[arg(long)]
        keep_binary: bool,
        /// Keep the download cache.
        #[arg(long)]
        keep_downloads: bool,
    },
    /// Bootstrap resilience test: wipe, reinstall, re-pull and verify, with timings.
    Rebuild {
        /// Keep the installed distribution.
        #[arg(long)]
        keep_binary: bool,
        /// Keep the download cache.
        #[arg(long)]
        keep_downloads: bool,
    },
}

/// Entry point of the `anchor` binary.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("anchor: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("anchor: {err}");
            ExitCode::FAILURE
        }
    }
}

fn load_config(path: &Option<PathBuf>) -> Result<(AnchorConfig, PathBuf)> {
    let path = path.clone().unwrap_or_else(default_config_path);
    Ok((AnchorConfig::load_or_default(&path)?, path))
}

async fn open_engine(config: AnchorConfig) -> Result<Arc<Engine>> {
    tokio::task::spawn_blocking(move || Engine::open(config)).await?
}

/// The engine's store as a sync store; applied units flow into the in-memory graph. As a
/// client, units are materialised only as deep as this device holds each source.
fn graph_unit_store(engine: &Arc<Engine>, client: bool) -> Arc<GraphUnitStore> {
    let sink = engine.clone();
    let store = GraphUnitStore::new(
        engine.store().clone(),
        Arc::new(move |outcome| {
            if let Err(err) = sink.absorb(outcome) {
                tracing::warn!("cannot absorb a synced unit: {err}");
            }
        }),
    );
    if !client {
        return Arc::new(store);
    }
    let depths = engine.clone();
    let default = engine.config().sync.default_depth;
    Arc::new(store.with_depths(Arc::new(move |source| {
        depths.depth_or_record(source, default)
    })))
}

/// Client options with the depth this device holds each known source at.
fn client_options(engine: &Arc<Engine>, token: Option<String>) -> Result<ClientOptions> {
    let keys_dir = engine.paths().keys_dir();
    let identity = Arc::new(DeviceIdentity::load_or_create(&keys_dir)?);
    let mut options = ClientOptions::from_config(
        &engine.config().sync,
        identity,
        token,
        trust_policy(&keys_dir)?,
    )?;
    options.default_depth = engine.config().sync.default_depth;
    let mut known: Vec<SourceId> = engine
        .config()
        .sources
        .iter()
        .map(|s| s.id.clone())
        .collect();
    known.extend(engine.store().sources()?.into_iter().map(|s| s.id));
    for id in known {
        let depth = engine.depth(&id);
        options.depths.insert(id, depth);
    }
    Ok(options)
}

/// In client mode, lets the engine pull deeper tiles of remote sources from the server.
fn install_remote(engine: &Arc<Engine>) -> Result<()> {
    let sync = &engine.config().sync;
    if engine.config().mode != crate::config::Mode::Client || sync.server_url.is_none() {
        return Ok(());
    }
    let token = token_from_env(sync.auth_token_env.as_deref(), |name| {
        std::env::var(name).ok()
    });
    let options = client_options(engine, token)?;
    engine.set_remote(Arc::new(SyncTiles::new(
        graph_unit_store(engine, true),
        options,
        tokio::runtime::Handle::current(),
    )));
    Ok(())
}

/// Trusted device keys when any are configured; otherwise every valid signature is accepted
/// (the bearer token still gates access).
fn trust_policy(keys_dir: &std::path::Path) -> Result<TrustPolicy> {
    let devices = TrustedDevices::open(keys_dir)?;
    Ok(if devices.list().is_empty() {
        TrustPolicy::AnyValid
    } else {
        devices.policy()
    })
}

fn query_engine(engine: &Arc<Engine>) -> QueryEngine {
    let depths: Arc<dyn crate::engine::query::DepthControl> = engine.clone();
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
    .with_depths(depths)
}

fn print_progress(event: ProvisionEvent) {
    match event {
        ProvisionEvent::Step { .. } => println!("{event:?}"),
        ProvisionEvent::Download {
            received, total, ..
        } => {
            if let Some(total) = total.filter(|total| *total > 0) {
                print!("\r  downloading {:>3}%", received * 100 / total);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        _ => {}
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init { force } => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            if path.exists() && !force {
                return Err(Error::invalid(format!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                )));
            }
            AnchorConfig::default().save(&path)?;
            println!(
                "Wrote {}. Add [[sources]] entries, then run `anchor ingest`.",
                path.display()
            );
        }
        Command::Setup { yes, rebuild_test } => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            let existing = if path.exists() {
                Some(AnchorConfig::load(&path)?)
            } else {
                None
            };
            let env = wizard::WizardEnvironment::real(path, existing);
            let options = wizard::WizardOptions {
                yes,
                rebuild_test,
                ..wizard::WizardOptions::default()
            };
            let outcome =
                wizard::run_wizard(&wizard::InquirePrompter::new(), &env, &options).await?;
            if let Some(test) = &outcome.rebuild_test
                && (test.refused.is_some() || test.error.is_some())
            {
                return Err(Error::invalid(
                    "the bootstrap resilience test did not run to completion (see above)",
                ));
            }
        }
        Command::Ingest { source } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            install_remote(&engine)?;
            let scan = engine.clone();
            let reports = tokio::task::spawn_blocking(move || match source {
                Some(id) => scan
                    .scan_source(&SourceId::new(id))
                    .map(|report| vec![report]),
                None => scan.scan_all(),
            })
            .await??;
            for report in &reports {
                println!(
                    "{:<24} {:>6} files  {:>6} units applied  {:>5} removed  {:>7} nodes  {:>7} edges  {:>6} ms  {} warnings",
                    report.source,
                    report.files_seen,
                    report.units_applied,
                    report.units_removed,
                    report.nodes_upserted,
                    report.edges_upserted,
                    report.duration_ms,
                    report.warnings.len()
                );
            }
            let embedded = engine.embed_pending().await?;
            println!("Embedded {embedded} chunks.");
            engine.flush()?;
        }
        Command::Query {
            text,
            json,
            local_only,
            retrieval_only,
            explain,
        } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            install_remote(&engine)?;
            let mut request = QueryRequest::new(text);
            request.explain = explain;
            request.mode = if retrieval_only {
                QueryMode::RetrievalOnly
            } else if local_only {
                QueryMode::LocalOnly
            } else {
                QueryMode::Auto
            };
            let response = query_engine(&engine).query(request).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                match &response.answer {
                    Some(answer) => println!("{answer}\n"),
                    None => println!("(no answer; showing retrieved context)\n"),
                }
                println!(
                    "confidence {:.2} · intent {} · {}",
                    response.confidence, response.intent.kind, response.route.reason
                );
                for item in response.context.items.iter().take(8) {
                    println!(
                        "  [{:?}] {} `{}` ({})",
                        item.role, item.node.name, item.node.key, item.node.trust
                    );
                }
                for area in &response.shallow {
                    println!("  shallow [{}]: {}", area.status, area.detail);
                }
                for gap in &response.gaps {
                    println!("  gap: {} — {}", gap.kind, gap.detail);
                }
            }
            engine.flush()?;
        }
        Command::Daemon { no_web } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let scan = engine.clone();
            let reports = tokio::task::spawn_blocking(move || scan.scan_all()).await??;
            println!("Initial scan: {} sources.", reports.len());
            engine.embed_pending().await?;
            let mut watcher = if engine.config().watcher.enabled {
                Some(FileWatcher::start(engine.clone())?)
            } else {
                None
            };
            // Embed new chunks shortly after graph changes, off the ingest path.
            let embedder = engine.clone();
            let mut events = engine.subscribe();
            let embedding = tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(EngineEvent::GraphChanged { .. }) => {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if let Err(err) = embedder.embed_pending().await {
                                tracing::warn!("embedding failed: {err}");
                            }
                            // Tag and summarise new material in the background, a little at a
                            // time; nobody waits on this.
                            if let Err(err) = embedder.enrich(25).await {
                                tracing::warn!("enrichment failed: {err}");
                            }
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            let web = if engine.config().web.enabled && !no_web {
                let addr = format!("{}:{}", engine.config().web.bind, engine.config().web.port);
                println!("Dashboard: http://{addr}");
                Some(tokio::spawn(crate::web::serve(
                    engine.clone(),
                    query_engine(&engine),
                )))
            } else {
                None
            };
            // Keep configured GitHub boards exported; changed files are picked up by re-scanning
            // the sources that read them.
            let mut refreshers = Vec::new();
            for entry in engine
                .config()
                .github
                .iter()
                .filter(|entry| entry.interval_minutes > 0)
            {
                let options = fetch::FetchOptions::from_config(entry, engine.paths());
                let period = Duration::from_secs(entry.interval_minutes.max(5) * 60);
                let engine = engine.clone();
                refreshers.push(tokio::spawn(async move {
                    let mut timer = tokio::time::interval(period);
                    loop {
                        timer.tick().await;
                        let (options, engine) = (options.clone(), engine.clone());
                        let repository = options.repository.clone();
                        let outcome =
                            tokio::task::spawn_blocking(move || -> Result<fetch::FetchReport> {
                                let report = fetch::fetch_github(&options)?;
                                if report.issues + report.discussions + report.pulls > 0 {
                                    for source in &engine.config().sources {
                                        if std::path::Path::new(&source.location)
                                            .starts_with(&options.out)
                                        {
                                            engine.scan_source(&source.id)?;
                                        }
                                    }
                                }
                                Ok(report)
                            })
                            .await;
                        match outcome {
                            Ok(Ok(report)) => tracing::info!("refreshed {repository}: {report:?}"),
                            Ok(Err(err)) => tracing::warn!("refreshing {repository} failed: {err}"),
                            Err(err) => tracing::warn!("refresh task failed: {err}"),
                        }
                    }
                }));
            }
            println!("Anchor is running. Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await?;
            embedding.abort();
            for refresher in refreshers {
                refresher.abort();
            }
            if let Some(web) = web {
                web.abort();
            }
            if let Some(watcher) = watcher.as_mut() {
                watcher.stop();
            }
            engine.flush()?;
        }
        Command::Status => {
            let (config, path) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let stats = engine.store().stats()?;
            println!("config   {}", path.display());
            println!("data     {}", engine.paths().root().display());
            println!(
                "graph    {} nodes, {} edges, {} units, {} chunks, {} pending references, {:.1} MB on disk",
                stats.node_count,
                stats.edge_count,
                stats.unit_count,
                stats.chunk_count,
                stats.pending_edges,
                stats.db_bytes as f64 / 1e6
            );
            for (kind, count) in &stats.nodes {
                println!("  {kind:<14} {count}");
            }
            for source in &engine.config().sources {
                println!(
                    "source   {:<24} {:<12} {:<16} depth {}",
                    source.id,
                    source.kind,
                    source.trust,
                    engine.depth(&source.id)
                );
            }
        }
        Command::Spend { days } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let summary = engine
                .ledger()
                .summary(TimeRange::last(Duration::from_secs(days * 86_400)))?;
            for locality in [crate::base::Locality::Local, crate::base::Locality::Cloud] {
                let stats = summary.locality(locality);
                println!("{locality:<6} {stats:?}");
            }
            println!(
                "savings from local calls: {}",
                format_usd(summary.savings_nanos)
            );
        }
        Command::Backlog => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            for item in engine.backlog().list(&Default::default())? {
                println!(
                    "#{:<5} {:<18} {:<12} hits {:<3} {}",
                    item.id,
                    item.kind,
                    item.status,
                    item.hits,
                    item.query.as_deref().unwrap_or(&item.gap_key)
                );
            }
        }
        Command::Export { output } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let exported = tokio::task::spawn_blocking(move || engine.export_pack(None)).await??;
            let path = match output {
                Some(output) => {
                    std::fs::copy(&exported, &output)?;
                    output
                }
                None => exported,
            };
            println!(
                "Wrote {} ({} bytes).",
                path.display(),
                std::fs::metadata(&path)?.len()
            );
        }
        Command::Import { file } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let applied = tokio::task::spawn_blocking(move || engine.import_pack(&file)).await??;
            println!("Applied {applied} unit versions.");
        }
        Command::Server => {
            let (config, _) = load_config(&cli.config)?;
            let listen = config.sync.listen.clone();
            let token = token_from_env(config.sync.auth_token_env.as_deref(), |name| {
                std::env::var(name).ok()
            });
            let engine = open_engine(config).await?;
            let keys_dir = engine.paths().keys_dir();
            let identity = Arc::new(DeviceIdentity::load_or_create(&keys_dir)?);
            let mut options = ServerOptions::new(identity, token);
            options.trust = trust_policy(&keys_dir)?;
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            println!(
                "Sync server listening on {} (device {}).",
                listener.local_addr()?,
                options.identity.device_id()
            );
            SyncServer::serve(listener, graph_unit_store(&engine, false), options, async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        }
        Command::Sync => {
            let (config, _) = load_config(&cli.config)?;
            let token = token_from_env(config.sync.auth_token_env.as_deref(), |name| {
                std::env::var(name).ok()
            });
            let engine = open_engine(config).await?;
            let options = client_options(&engine, token)?;
            let report = SyncClient::new(graph_unit_store(&engine, true), options)
                .sync_once()
                .await?;
            println!(
                "Pushed {} and pulled {} unit versions ({} rejected, {} bytes) in {:.2?}.",
                report.pushed, report.pulled, report.rejected, report.bytes, report.duration
            );
        }
        Command::Demo { out, scale, seed } => {
            let preset = match scale {
                DemoScale::Tiny => crate::synth::Preset::Tiny,
                DemoScale::Small => crate::synth::Preset::Small,
                DemoScale::Medium => crate::synth::Preset::Medium,
            };
            let spec = crate::synth::SynthSpec::preset(preset).seed(seed);
            let target = out.clone();
            let report =
                tokio::task::spawn_blocking(move || crate::synth::generate(&spec, &target))
                    .await??;
            println!(
                "Generated {} repositories ({} source files), {} wiki pages, {} issues, {} pull requests and {} questions ({:.1} MB) in {}.",
                report.repositories,
                report.source_files,
                report.wiki_pages,
                report.tickets,
                report.pull_requests,
                report.total_questions(),
                report.total_bytes() as f64 / 1e6,
                out.display()
            );
            let config = out.join("anchor.toml");
            println!("\nIngest it (no model or network needed):");
            println!("  anchor --config {} ingest", config.display());
            let example = std::fs::read_to_string(out.join("ground_truth.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<crate::synth::GroundTruth>(&text).ok())
                .and_then(|truth| truth.ownership.first().map(|q| q.question.clone()))
                .unwrap_or_else(|| "Who owns the payments-api service?".to_owned());
            println!("Then ask:");
            println!(
                "  anchor --config {} query \"{example}\" --retrieval-only",
                config.display()
            );
            println!(
                "Questions with known answers are in {}.",
                out.join("ground_truth.json").display()
            );
        }
        Command::Deepen { source, depth } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            install_remote(&engine)?;
            let scan = engine.clone();
            let report =
                tokio::task::spawn_blocking(move || scan.deepen(&SourceId::new(source), depth))
                    .await??;
            println!(
                "{} is now at {} depth: {} files, {} units applied, {} nodes, {} edges in {} ms.",
                report.source,
                report.depth,
                report.files_seen,
                report.units_applied,
                report.nodes_upserted,
                report.edges_upserted,
                report.duration_ms
            );
            engine.embed_pending().await?;
            engine.flush()?;
        }
        Command::Fetch {
            repository,
            out,
            issues,
            discussions,
            pulls,
        } => {
            let (config, _) = load_config(&cli.config)?;
            let Some(repository) = repository else {
                if config.github.is_empty() {
                    return Err(Error::invalid(
                        "name a repository (owner/name) or add [[github]] entries to the configuration",
                    ));
                }
                for entry in &config.github {
                    let options = fetch::FetchOptions::from_config(entry, &config.paths());
                    let report = tokio::task::spawn_blocking(move || fetch::fetch_github(&options))
                        .await??;
                    println!(
                        "{}: {} issues, {} discussions, {} pull requests new or updated ({} unchanged).",
                        entry.repository,
                        report.issues,
                        report.discussions,
                        report.pulls,
                        report.skipped
                    );
                }
                return Ok(());
            };
            let out = out.unwrap_or_else(|| {
                config
                    .paths()
                    .root()
                    .join("github")
                    .join(repository.replace('/', "-"))
            });
            let options = fetch::FetchOptions {
                repository: repository.clone(),
                out: out.clone(),
                issues,
                discussions,
                pulls,
            };
            let report =
                tokio::task::spawn_blocking(move || fetch::fetch_github(&options)).await??;
            println!(
                "Exported {} issues, {} discussions and {} pull requests from {repository} ({} unchanged) into {}.",
                report.issues,
                report.discussions,
                report.pulls,
                report.skipped,
                out.display()
            );
            let id = crate::base::keys::slugify(&repository);
            println!("\nAdd these sources to your configuration, then run `anchor ingest`:\n");
            for (suffix, kind, dir) in [
                ("issues", "issues", "issues"),
                ("discussions", "issues", "discussions"),
                ("reviews", "code_review", "pulls"),
            ] {
                println!(
                    "[[sources]]\nid = \"{id}-{suffix}\"\nkind = \"{kind}\"\ntrust = \"reference_only\"\nlocation = \"{}\"\n",
                    out.join(dir).display()
                );
            }
            println!(
                "To keep it refreshed by the daemon, also add:\n\n[[github]]\nrepository = \"{repository}\"\ninterval_minutes = 60"
            );
        }
        Command::Eval {
            truth,
            local_only,
            retrieval_only,
            json,
        } => {
            let (config, _) = load_config(&cli.config)?;
            let truth: crate::synth::GroundTruth =
                serde_json::from_str(&std::fs::read_to_string(&truth)?)?;
            let engine = open_engine(config).await?;
            let mode = if retrieval_only {
                QueryMode::RetrievalOnly
            } else if local_only {
                QueryMode::LocalOnly
            } else {
                QueryMode::Auto
            };
            let report =
                eval::evaluate(&query_engine(&engine), &engine.ledger(), &truth, mode).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", eval::render(&report));
            }
            engine.flush()?;
        }
        Command::Enrich { limit } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            if engine
                .router()
                .model_info(crate::base::ModelTier::System1)
                .is_none()
            {
                return Err(Error::invalid(
                    "enrichment needs a System 1 model: set [models.system1] (see `anchor ollama provision`)",
                ));
            }
            let report = engine.enrich(limit).await?;
            println!(
                "Enriched {} nodes ({} unchanged, {} failed, {} left for a later run).",
                report.enriched, report.unchanged, report.failed, report.remaining
            );
            engine.embed_pending().await?;
            engine.flush()?;
        }
        Command::Ollama { action } => {
            let (mut config, path) = load_config(&cli.config)?;
            let manager = OllamaManager::new(config.ollama.clone(), &config.paths());
            match action {
                OllamaAction::Status => println!("{:#?}", manager.status().await),
                OllamaAction::Provision { model } => {
                    let options = ProvisionOptions {
                        model: Some(model.unwrap_or_else(|| config.ollama.default_model.clone())),
                        ..ProvisionOptions::default()
                    };
                    let report = manager.ensure_ready(&options, &print_progress).await?;
                    println!(
                        "\nOllama ready at {} (model {}).",
                        report.base_url, report.model
                    );
                    config.ollama.base_url = report.base_url.clone();
                    config.ollama.managed = report.installed || report.started;
                    config.save(&path)?;
                }
                OllamaAction::Teardown {
                    keep_binary,
                    keep_downloads,
                } => {
                    let options = TeardownOptions {
                        keep_distribution: keep_binary,
                        keep_cache: keep_downloads,
                        ..TeardownOptions::default()
                    };
                    println!("{:#?}", manager.teardown(&options).await?);
                }
                OllamaAction::Rebuild {
                    keep_binary,
                    keep_downloads,
                } => {
                    let options = RebuildOptions {
                        teardown: TeardownOptions {
                            keep_distribution: keep_binary,
                            keep_cache: keep_downloads,
                            ..TeardownOptions::default()
                        },
                        provision: ProvisionOptions {
                            model: Some(config.ollama.default_model.clone()),
                            ..ProvisionOptions::default()
                        },
                    };
                    println!("\n{:#?}", manager.rebuild(&options, &print_progress).await?);
                }
            }
        }
    }
    Ok(())
}
