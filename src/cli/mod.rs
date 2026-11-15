//! Command-line interface (`anchor <command>`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

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
    /// Manage the local Ollama backend.
    Ollama {
        #[command(subcommand)]
        action: OllamaAction,
    },
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
        } => {
            let (config, _) = load_config(&cli.config)?;
            let engine = open_engine(config).await?;
            let mut request = QueryRequest::new(text);
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
            println!("Anchor is running. Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await?;
            embedding.abort();
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
                    "source   {:<24} {:<12} {}",
                    source.id, source.kind, source.trust
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
