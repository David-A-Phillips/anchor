//! Command-line interface (`anchor <command>`).

pub mod atlassian;
pub mod compare;
mod daemon;
mod dist;
pub mod eval;
pub mod fetch;
pub mod source;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::base::{Error, NodeKind, Result, SourceId, SourceKind};
use crate::config::provision::{
    OllamaManager, ProvisionEvent, ProvisionOptions, RebuildOptions, TeardownOptions,
};
use crate::config::wizard;
use crate::config::{AnchorConfig, default_config_path};
use crate::engine::Engine;
use crate::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use crate::engine::watcher::FileWatcher;
use crate::metrics::{TimeRange, format_usd};
use crate::sync::identity::{DeviceIdentity, TrustPolicy, TrustedDevices};
use crate::sync::{
    ClientOptions, GraphUnitStore, ServerOptions, SyncClient, SyncServer, SyncTiles, token_from_env,
};

/// Anchor: a local-first context engine that grounds AI models in your code, docs and org.
#[derive(Debug, Parser)]
#[command(
    name = "anchor",
    version,
    about = "One graph of your code, docs, tickets and org chart that answers who owns it, why it is this way, and what depends on it.",
    after_help = "Getting started:\n  anchor demo --out ~/anchor-demo     try it on a generated organisation\n  anchor source add <path>            index one of your repositories\n\nDocumentation: https://github.com/David-A-Phillips/anchor#readme"
)]
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
        /// Ask only for the entities each text refers to and link them; no summaries, tags or
        /// questions. About a quarter of the model output of a full pass.
        #[arg(long)]
        links_only: bool,
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
    /// Explain why code is the way it is: blame the lines, then follow each commit to its pull
    /// request, review decisions, tickets and the pages that discuss them.
    Why {
        /// `path`, `path:line`, `path:start-end`, `source:path[:lines]` or a symbol name.
        target: String,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What could break: everything that depends on any of these files, symbols or entities,
    /// and the owners to notify.
    Impact {
        /// Files (`path`, `source:path`), symbols, entity keys (`service:payments-api`) or names.
        targets: Vec<String>,
        /// Also take every file changed since this git revision in the current repository
        /// (default `HEAD`: uncommitted changes).
        #[arg(long, value_name = "REV", num_args = 0..=1, default_missing_value = "HEAD")]
        diff: Option<String>,
        /// Only report dependents of this kind (repeatable): service, file, symbol, api_spec, ...
        #[arg(long = "kind", value_name = "KIND")]
        kinds: Vec<String>,
        /// Maximum dependents listed.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Print the report as JSON.
        #[arg(long, conflicts_with = "markdown")]
        json: bool,
        /// Print the report as Markdown, for a pull-request comment.
        #[arg(long)]
        markdown: bool,
    },
    /// The most depended-on services (or files, symbols, ...), who owns them, and which have no
    /// owner or a single-person owner.
    Hotspots {
        /// Granularity: service, component, api_spec, api_endpoint, file or symbol.
        #[arg(long, default_value = "service")]
        kind: String,
        /// Entries listed.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only list entities with at least this many dependents.
        #[arg(long, default_value_t = 1)]
        min_dependents: usize,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare Anchor with plain retrieval and with other RAG systems on the same questions.
    Compare {
        /// Working directory: `questions.jsonl` and `results/<system>.jsonl`.
        dir: PathBuf,
        /// Ground truth to (re)generate the questions and Anchor's results from. Without it,
        /// only the results already in the directory are scored.
        #[arg(long)]
        truth: Option<PathBuf>,
        /// Also break the scores down by question kind.
        #[arg(long)]
        kinds: bool,
        /// With --truth: ingest every source first and record how long indexing takes.
        #[arg(long, requires = "truth")]
        reindex: bool,
        /// Print the scores as JSON.
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
    /// Export live Confluence pages and Jira issues (see docs/atlassian.md).
    FetchAtlassian(atlassian::FetchAtlassianArgs),
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
    /// Serve the Model Context Protocol on stdio, for agent hosts such as Claude Code.
    Mcp {
        /// Optional management action; plain `anchor mcp` serves.
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// List, add or remove sources without editing the configuration by hand.
    Source {
        #[command(subcommand)]
        action: source::SourceAction,
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
        /// Do not give the repositories a git history (as the benchmark corpus is built).
        #[arg(long)]
        no_history: bool,
    },
    /// Manage the local Ollama backend.
    Ollama {
        #[command(subcommand)]
        action: OllamaAction,
    },
    /// Print a shell completion script (used when packaging).
    #[command(hide = true)]
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: dist::CompletionShell,
    },
    /// Print the man page in roff format (used when packaging).
    #[command(hide = true)]
    Man,
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
    // Output piped into a reader that stops early (`anchor ... | head`) is not an error: exit
    // quietly instead of panicking on the closed pipe.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.contains("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
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

/// Actions of `anchor mcp`.
#[derive(Subcommand, Debug)]
enum McpAction {
    /// Register this binary as an MCP server with Claude Code, Claude Desktop or Cursor.
    Install(crate::mcp::install::InstallArgs),
    /// Remove the registration made by `install`.
    Uninstall(crate::mcp::install::UninstallArgs),
    /// Print the agent skill (when to call Anchor, how to keep it current), e.g. to paste into
    /// an `AGENTS.md` or a rules file for hosts without skill support.
    Skill,
}

fn run_mcp_action(action: McpAction, config: Option<&Path>) -> Result<()> {
    use crate::mcp::install;
    let locations = install::Locations::system()?;
    let lines = match action {
        McpAction::Install(args) => {
            let exe = std::env::current_exe()
                .map_err(|err| Error::config(format!("cannot locate this executable: {err}")))?;
            let config = config.map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));
            install::install(
                &args,
                &exe,
                config.as_deref(),
                &locations,
                &install::SystemRunner,
            )?
        }
        McpAction::Uninstall(args) => {
            install::uninstall(&args, &locations, &install::SystemRunner)?
        }
        McpAction::Skill => vec![install::SKILL.to_owned()],
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Files changed since `rev` in the git repository containing `cwd`, plus untracked new files, as
/// targets for `anchor impact`: absolute paths for files that exist, `source:path` for deleted
/// files that live in a configured repository (their nodes are still in the graph until the next
/// ingest). Sorted and deduplicated.
pub(crate) fn changed_files(rev: &str, config: &AnchorConfig, cwd: &Path) -> Result<Vec<String>> {
    let git = |dir: &Path, args: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()?;
        if !output.status.success() {
            return Err(Error::invalid(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let top = match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => PathBuf::from(top.trim()),
        Err(_) => {
            return Err(Error::invalid(format!(
                "--diff takes the changed files from git, but {} is not inside a git repository",
                cwd.display()
            )));
        }
    };
    let roots: Vec<(String, PathBuf)> = config
        .sources
        .iter()
        .filter(|source| source.enabled && source.kind == SourceKind::Repo)
        .filter_map(|source| {
            let root = PathBuf::from(&source.location).canonicalize().ok()?;
            Some((source.id.to_string(), root))
        })
        .collect();
    let top = top.canonicalize().unwrap_or(top);
    // Changed tracked files (relative to the top), then new files git does not track yet.
    let mut changed: Vec<String> = git(&top, &["diff", "--name-only", rev, "--"])?
        .lines()
        .map(str::to_owned)
        .collect();
    changed.extend(
        git(&top, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_owned),
    );
    let mut out = Vec::new();
    for line in &changed {
        let rel = line.trim();
        if rel.is_empty() {
            continue;
        }
        let path = top.join(rel);
        if path.is_file() {
            out.push(path.to_string_lossy().into_owned());
            continue;
        }
        let in_source = roots
            .iter()
            .filter_map(|(id, root)| Some((id, path.strip_prefix(root).ok()?)))
            .max_by_key(|(_, rest)| std::cmp::Reverse(rest.components().count()));
        match in_source {
            Some((id, rest)) => out.push(format!(
                "{id}:{}",
                rest.to_string_lossy().replace('\\', "/")
            )),
            None => out.push(path.to_string_lossy().into_owned()),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The configuration written on a first run (`init`, or `source add` without a config file):
/// the defaults, minus the local model tier when no model server answers, so questions never
/// wait on a server that is not there. Returns the note to print about it.
async fn first_run_config() -> (AnchorConfig, Option<String>) {
    let mut config = AnchorConfig::default();
    let url = config
        .models
        .system1
        .base_url
        .clone()
        .unwrap_or_else(|| config.ollama.base_url.clone());
    let reachable = match crate::ollama::OllamaApi::new(&url) {
        Ok(api) => {
            api.with_probe_timeout(Duration::from_millis(300))
                .is_reachable()
                .await
        }
        Err(_) => false,
    };
    if reachable {
        return (config, None);
    }
    config.models.system1.provider = crate::engine::router::ProviderKind::Disabled;
    (
        config,
        Some(format!(
            "No local model server answered at {url}, so none is configured: questions are \
             classified by rules and answered from the graph (no model is needed). `anchor setup` \
             adds one later."
        )),
    )
}

/// Tells a new user, on stderr, why there is nothing to answer from. Returns `true` when nothing
/// is configured (no file, or a standalone configuration without sources).
fn hint_if_unconfigured(config: &AnchorConfig, path: &Path) -> bool {
    let missing = !path.exists();
    let empty = config.sources.is_empty() && config.mode == crate::config::Mode::Standalone;
    if !missing && !empty {
        return false;
    }
    if missing {
        eprintln!(
            "No configuration at {}: nothing is indexed yet.",
            path.display()
        );
    } else {
        eprintln!("No sources in {}: nothing is indexed yet.", path.display());
    }
    eprintln!(
        "Start with `anchor source add <path-to-a-repository>`, or try sample data with \
         `anchor demo --out <dir>`."
    );
    true
}

fn load_config(path: Option<&Path>) -> Result<(AnchorConfig, PathBuf)> {
    let path = path.map_or_else(default_config_path, Path::to_path_buf);
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
        Command::Completions { shell } => dist::completions::<Cli>(shell),
        Command::Man => dist::man::<Cli>()?,
        Command::Init { force } => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            if path.exists() && !force {
                return Err(Error::invalid(format!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                )));
            }
            let (config, note) = first_run_config().await;
            config.save(&path)?;
            if let Some(note) = note {
                println!("{note}");
            }
            println!(
                "Wrote {}.\nNext: `anchor source add <path>` to index a repository (or uncomment the \
                 [[sources]] example at the end of the file and run `anchor ingest`).",
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
            let (config, path) = load_config(cli.config.as_deref())?;
            if hint_if_unconfigured(&config, &path) {
                return Ok(());
            }
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
            let (config, path) = load_config(cli.config.as_deref())?;
            hint_if_unconfigured(&config, &path);
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
                    None => match response
                        .gaps
                        .iter()
                        .find(|gap| gap.kind == "unresolved_entity")
                    {
                        Some(gap) => println!(
                            "No answer: {}. Check the spelling, or see what is indexed with `anchor status`.\n",
                            gap.detail
                        ),
                        None => println!(
                            "No direct answer in the graph; the closest evidence follows.\n"
                        ),
                    },
                }
                let source = match response.route.tier {
                    Some(tier) => format!("written by the {tier} model"),
                    None => "from the graph".to_owned(),
                };
                println!(
                    "confidence {:.2} · {} question · {source}",
                    response.confidence, response.intent.kind
                );
                for item in response.context.items.iter().take(8) {
                    let role = match item.role {
                        crate::engine::query::ItemRole::Answer => "answer",
                        crate::engine::query::ItemRole::Evidence => "evidence",
                        crate::engine::query::ItemRole::Overridden => "overridden",
                    };
                    println!(
                        "  [{role}] {} `{}` ({})",
                        item.node.name, item.node.key, item.node.trust
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
            let (config, _) = load_config(cli.config.as_deref())?;
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
            let embedding = daemon::spawn_embed_loop(&engine);
            let web = if engine.config().web.enabled && !no_web {
                let addr = format!("{}:{}", engine.config().web.bind, engine.config().web.port);
                println!("Dashboard: http://{addr}");
                let (served, queries) = (engine.clone(), query_engine(&engine));
                Some(tokio::spawn(async move {
                    // A dashboard that cannot start (port taken, bad bind address) must not die
                    // silently inside a detached task.
                    if let Err(err) = crate::web::serve(served, queries).await {
                        tracing::error!("the dashboard stopped: {err}");
                        eprintln!("anchor: the dashboard on {addr} stopped: {err}");
                    }
                }))
            } else {
                None
            };
            // Keep configured GitHub boards exported; changed files are picked up by re-scanning
            // the sources that read them.
            let mut refreshers = daemon::spawn_github_refreshers(&engine);
            refreshers.extend(atlassian::spawn_refreshers(&engine));
            println!("Anchor is running. Press Ctrl-C to stop.");
            daemon::shutdown_signal().await;
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
            let (config, path) = load_config(cli.config.as_deref())?;
            let engine = open_engine(config).await?;
            let stats = engine.store().stats()?;
            println!(
                "config   {}{}",
                path.display(),
                if path.exists() {
                    ""
                } else {
                    " (not created yet)"
                }
            );
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
            let opened = engine.graph_open();
            println!(
                "loaded   in {} ms, {}",
                opened.millis,
                if opened.reloaded {
                    "rebuilt from the database".to_owned()
                } else if opened.replayed > 0 {
                    format!(
                        "from the saved graph plus {} logged changes",
                        opened.replayed
                    )
                } else {
                    "from the saved graph".to_owned()
                }
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
            let (config, _) = load_config(cli.config.as_deref())?;
            let engine = open_engine(config).await?;
            let summary = engine
                .ledger()
                .summary(TimeRange::last(Duration::from_secs(
                    days.saturating_mul(86_400),
                )))?;
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
            let (config, _) = load_config(cli.config.as_deref())?;
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
            let (config, _) = load_config(cli.config.as_deref())?;
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
            let (config, _) = load_config(cli.config.as_deref())?;
            let engine = open_engine(config).await?;
            let applied = tokio::task::spawn_blocking(move || engine.import_pack(&file)).await??;
            println!("Applied {applied} unit versions.");
        }
        Command::Mcp {
            action: Some(action),
        } => run_mcp_action(action, cli.config.as_deref())?,
        Command::Mcp { action: None } => {
            let (config, _) = load_config(cli.config.as_deref())?;
            let engine = open_engine(config).await?;
            install_remote(&engine)?;
            let server = crate::mcp::McpServer::new(engine.clone(), query_engine(&engine));
            server.serve_stdio().await?;
            engine.flush()?;
        }
        Command::Server => {
            let (config, _) = load_config(cli.config.as_deref())?;
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
                daemon::shutdown_signal().await;
            })
            .await?;
        }
        Command::Sync => {
            let (config, _) = load_config(cli.config.as_deref())?;
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
        Command::Demo {
            out,
            scale,
            seed,
            no_history,
        } => {
            let preset = match scale {
                DemoScale::Tiny => crate::synth::Preset::Tiny,
                DemoScale::Small => crate::synth::Preset::Small,
                DemoScale::Medium => crate::synth::Preset::Medium,
            };
            let spec = crate::synth::SynthSpec::preset(preset).seed(seed);
            let target = out.clone();
            let generated = spec.clone();
            let report =
                tokio::task::spawn_blocking(move || crate::synth::generate(&generated, &target))
                    .await??;
            // A git history makes `anchor why` work on the sample data too.
            let history = if no_history {
                None
            } else if crate::synth::git_available() {
                let target = std::fs::canonicalize(&out)?;
                match tokio::task::spawn_blocking(move || crate::synth::init_git(&spec, &target))
                    .await?
                {
                    Ok(history) => Some(history),
                    Err(err) => {
                        eprintln!(
                            "note: could not create the git history ({err}); `anchor why` will have no commits to show"
                        );
                        None
                    }
                }
            } else {
                eprintln!(
                    "note: git is not installed, so the sample repositories have no history for `anchor why`"
                );
                None
            };
            println!(
                "Generated {} repositories ({} source files{}), {} wiki pages, {} issues, {} pull requests and {} questions ({:.1} MB) in {}.",
                report.repositories,
                report.source_files,
                history
                    .as_ref()
                    .map(|h| format!(", {} commits", h.commits))
                    .unwrap_or_default(),
                report.wiki_pages,
                report.tickets,
                report.pull_requests,
                report.total_questions(),
                report.total_bytes() as f64 / 1e6,
                out.display()
            );
            let config = std::fs::canonicalize(out.join("anchor.toml"))?;
            let anchor = format!("anchor --config {}", config.display());
            let truth = std::fs::read_to_string(out.join("ground_truth.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<crate::synth::GroundTruth>(&text).ok());
            let ownership = truth
                .as_ref()
                .and_then(|truth| truth.ownership.first())
                .map(|q| (q.question.clone(), q.subject_key.clone()));
            println!("\nIngest it (no model or network needed):");
            println!("  {anchor} ingest");
            println!("\nThen try:");
            if let Some((question, subject)) = &ownership {
                println!("  {anchor} query \"{question}\"");
                println!("  {anchor} impact {subject}");
            }
            if let Some(example) = history.as_ref().and_then(|h| h.example.clone()) {
                println!("  {anchor} why {example}");
            }
            println!("  {anchor} hotspots");
            println!(
                "\nQuestions with known answers are in {}.",
                std::fs::canonicalize(out.join("ground_truth.json"))?.display()
            );
        }
        Command::Source { action } => {
            let (mut config, path) = load_config(cli.config.as_deref())?;
            if !path.exists() && matches!(action, source::SourceAction::Add { .. }) {
                let (fresh, note) = first_run_config().await;
                config = fresh;
                println!("Creating {}.", path.display());
                if let Some(note) = note {
                    println!("{note}");
                }
            }
            let lines =
                tokio::task::spawn_blocking(move || source::run(action, config, &path)).await??;
            for line in lines {
                println!("{line}");
            }
        }
        Command::Deepen { source, depth } => {
            let (config, _) = load_config(cli.config.as_deref())?;
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
            let (config, _) = load_config(cli.config.as_deref())?;
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
        Command::FetchAtlassian(args) => {
            let (config, _) = load_config(cli.config.as_deref())?;
            atlassian::run_command(&config, args).await?;
        }
        Command::Eval {
            truth,
            local_only,
            retrieval_only,
            json,
        } => {
            let (config, _) = load_config(cli.config.as_deref())?;
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
        Command::Why { target, json } => {
            let (config, path) = load_config(cli.config.as_deref())?;
            hint_if_unconfigured(&config, &path);
            let engine = open_engine(config).await?;
            let asked = engine.clone();
            let report = tokio::task::spawn_blocking(move || asked.why(&target)).await??;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", crate::engine::why::render(&report));
            }
        }
        Command::Impact {
            mut targets,
            diff,
            kinds,
            limit,
            json,
            markdown,
        } => {
            let (config, path) = load_config(cli.config.as_deref())?;
            hint_if_unconfigured(&config, &path);
            let from_diff = diff.is_some();
            if let Some(rev) = &diff {
                let changed = changed_files(rev, &config, &std::env::current_dir()?)?;
                if changed.is_empty() && targets.is_empty() {
                    if json {
                        let empty = crate::engine::impact::ImpactReport::default();
                        println!("{}", serde_json::to_string_pretty(&empty)?);
                    } else {
                        println!("No files changed since {rev}: nothing to check.");
                    }
                    return Ok(());
                }
                targets.extend(changed);
            }
            if targets.is_empty() {
                return Err(Error::invalid(
                    "name files, symbols or entities, or use --diff to take the files changed in git",
                ));
            }
            let options = crate::engine::impact::ImpactOptions {
                limit: limit.max(1),
                kinds: kinds
                    .iter()
                    .map(|kind| kind.parse::<NodeKind>())
                    .collect::<Result<_>>()?,
                ..Default::default()
            };
            let engine = open_engine(config).await?;
            let report =
                match tokio::task::spawn_blocking(move || engine.impact(&targets, &options)).await?
                {
                    Ok(report) => report,
                    // Changed files that are not in the graph (configuration, docs Anchor does not
                    // index, files outside every source) have nothing depending on them.
                    Err(Error::NotFound(detail)) if from_diff => {
                        if json {
                            let report = crate::engine::impact::ImpactReport {
                                gaps: vec![format!(
                                    "none of the changed files is in the graph: {detail}"
                                )],
                                ..Default::default()
                            };
                            println!("{}", serde_json::to_string_pretty(&report)?);
                        } else {
                            println!("None of the changed files is in the graph: {detail}");
                        }
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if markdown {
                print!("{}", crate::engine::impact::render_impact_markdown(&report));
            } else {
                print!("{}", crate::engine::impact::render_impact(&report));
            }
        }
        Command::Hotspots {
            kind,
            limit,
            min_dependents,
            json,
        } => {
            let (config, path) = load_config(cli.config.as_deref())?;
            hint_if_unconfigured(&config, &path);
            let options = crate::engine::impact::HotspotOptions {
                kind: kind.parse::<NodeKind>()?,
                limit: limit.max(1),
                min_dependents,
            };
            let engine = open_engine(config).await?;
            let report = tokio::task::spawn_blocking(move || engine.hotspots(&options)).await??;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", crate::engine::impact::render_hotspots(&report));
            }
        }
        Command::Compare {
            dir,
            truth,
            kinds,
            reindex,
            json,
        } => {
            if let Some(truth) = truth {
                let (config, _) = load_config(cli.config.as_deref())?;
                let truth: crate::synth::GroundTruth =
                    serde_json::from_str(&std::fs::read_to_string(&truth)?)?;
                let engine = open_engine(config).await?;
                // With --reindex the sources are ingested from scratch first, so that indexing
                // time is measured too (start from an empty data directory).
                let mut index_seconds = None;
                if reindex {
                    let started = std::time::Instant::now();
                    let scan = engine.clone();
                    tokio::task::spawn_blocking(move || scan.scan_all()).await??;
                    engine.embed_pending().await?;
                    index_seconds = Some(started.elapsed().as_secs_f64());
                }
                compare::export(&engine, &query_engine(&engine), &truth, &dir, index_seconds)
                    .await?;
                engine.flush()?;
            }
            let board = compare::score_dir(&dir, kinds)?;
            let footprints = compare::footprints(&dir)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "scores": board, "footprint": footprints })
                    )?
                );
            } else {
                print!("{}", compare::render(&board));
                print!("{}", compare::render_footprints(&footprints));
            }
        }
        Command::Enrich { limit, links_only } => {
            let (config, _) = load_config(cli.config.as_deref())?;
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
            let mode = if links_only {
                crate::engine::enrich::EnrichMode::Links
            } else {
                crate::engine::enrich::EnrichMode::Full
            };
            let report = engine.enrich_mode(limit, mode).await?;
            println!(
                "Enriched {} nodes ({} unchanged, {} failed, {} left for a later run).",
                report.enriched, report.unchanged, report.failed, report.remaining
            );
            engine.embed_pending().await?;
            engine.flush()?;
        }
        Command::Ollama { action } => {
            let (mut config, path) = load_config(cli.config.as_deref())?;
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

#[cfg(test)]
mod tests;
