//! The daemon's background work (`anchor daemon`).
//!
//! Each loop is split into a body that does one round of work and a thin driver that decides when
//! to run it, so the bodies can be exercised without sleeping, timers or signals.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;

use super::fetch::{self, FetchOptions, FetchReport};
use crate::base::Result;
use crate::engine::{Engine, EngineEvent};

/// Shortest refresh period the daemon honours, in minutes (a lower configured value is raised).
pub(crate) const MIN_REFRESH_MINUTES: u64 = 5;

/// Delay between a graph change and the embedding pass it triggers, so a burst of changes is
/// embedded once.
const EMBED_DEBOUNCE: Duration = Duration::from_millis(500);

/// Chunks tagged and summarised per background pass.
const ENRICH_BATCH: usize = 25;

/// The period of a refresher configured with `interval_minutes`: never shorter than
/// [`MIN_REFRESH_MINUTES`], and saturating instead of overflowing for absurd values.
pub(crate) fn refresh_period(interval_minutes: u64) -> Duration {
    Duration::from_secs(interval_minutes.max(MIN_REFRESH_MINUTES).saturating_mul(60))
}

/// Re-scans every configured source whose location lies under `out` (the directory a refresher
/// just exported into). Returns the ids of the sources scanned. Blocking work runs on the
/// blocking pool.
pub(crate) async fn rescan_under(engine: &Arc<Engine>, out: &Path) -> Result<Vec<String>> {
    let (engine, out) = (engine.clone(), out.to_path_buf());
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let mut scanned = Vec::new();
        for source in &engine.config().sources {
            if Path::new(&source.location).starts_with(&out) {
                engine.scan_source(&source.id)?;
                scanned.push(source.id.to_string());
            }
        }
        Ok(scanned)
    })
    .await?
}

/// One GitHub refresh: runs `fetch` (the `gh` export in production) on the blocking pool and, when
/// it wrote anything, re-scans the sources reading the export directory.
pub(crate) async fn refresh_github_once<F>(
    engine: &Arc<Engine>,
    options: &FetchOptions,
    fetch: F,
) -> Result<FetchReport>
where
    F: FnOnce(&FetchOptions) -> Result<FetchReport> + Send + 'static,
{
    let owned = options.clone();
    let report = tokio::task::spawn_blocking(move || fetch(&owned)).await??;
    if report.issues + report.discussions + report.pulls > 0 {
        rescan_under(engine, &options.out).await?;
    }
    Ok(report)
}

/// Starts one periodic refresher per `[[github]]` entry with `interval_minutes > 0`.
pub(crate) fn spawn_github_refreshers(engine: &Arc<Engine>) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for entry in engine
        .config()
        .github
        .iter()
        .filter(|entry| entry.interval_minutes > 0)
    {
        let options = FetchOptions::from_config(entry, engine.paths());
        let period = refresh_period(entry.interval_minutes);
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(period);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                timer.tick().await;
                match refresh_github_once(&engine, &options, fetch::fetch_github).await {
                    Ok(report) => {
                        tracing::info!("refreshed {}: {report:?}", options.repository);
                    }
                    Err(err) => tracing::warn!("refreshing {} failed: {err}", options.repository),
                }
            }
        }));
    }
    handles
}

/// One background pass: embeds the chunks created since the last one, then tags and summarises a
/// batch of new material. Failures are logged, never fatal; the two steps are independent.
pub(crate) async fn embed_and_enrich(engine: &Engine) {
    if let Err(err) = engine.embed_pending().await {
        tracing::warn!("embedding failed: {err}");
    }
    if let Err(err) = engine.enrich(ENRICH_BATCH).await {
        tracing::warn!("enrichment failed: {err}");
    }
}

/// Runs `work` after every [`EngineEvent::GraphChanged`] (waiting `debounce` first), until the
/// event channel closes. Other events are ignored; missed events (`Lagged`) are not a problem
/// because one pass handles everything pending.
pub(crate) async fn run_embed_loop<F, Fut>(
    mut events: broadcast::Receiver<EngineEvent>,
    debounce: Duration,
    mut work: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        match events.recv().await {
            Ok(EngineEvent::GraphChanged { .. }) => {
                if !debounce.is_zero() {
                    tokio::time::sleep(debounce).await;
                }
                work().await;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

/// Starts the embed-and-enrich loop on the engine's event stream.
pub(crate) fn spawn_embed_loop(engine: &Arc<Engine>) -> JoinHandle<()> {
    let events = engine.subscribe();
    let engine = engine.clone();
    tokio::spawn(run_embed_loop(events, EMBED_DEBOUNCE, move || {
        let engine = engine.clone();
        async move { embed_and_enrich(&engine).await }
    }))
}

/// Completes when the process is asked to stop: Ctrl-C everywhere, and SIGTERM on Unix (what
/// `systemd`, `docker stop` and `kill` send).
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::base::{Error, SourceKind};
    use crate::config::{AnchorConfig, SourceConfig};
    use crate::engine::router::ProviderKind;

    fn engine_with_export_source(dir: &Path) -> (Arc<Engine>, std::path::PathBuf) {
        let out = dir.join("export");
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(out.join("wiki")).expect("export dir");
        std::fs::create_dir_all(&elsewhere).expect("other dir");
        std::fs::write(out.join("wiki/a.md"), "# First\n\nOne.\n").expect("page");
        std::fs::write(elsewhere.join("b.md"), "# Other\n\nTwo.\n").expect("page");
        let mut config = AnchorConfig {
            data_dir: Some(dir.join("data")),
            ..AnchorConfig::default()
        };
        config.models.system1.provider = ProviderKind::Disabled;
        config.sources.push(SourceConfig::new(
            "exported",
            SourceKind::Wiki,
            out.join("wiki").to_string_lossy(),
        ));
        config.sources.push(SourceConfig::new(
            "other",
            SourceKind::Wiki,
            elsewhere.to_string_lossy(),
        ));
        (Engine::open(config).expect("engine"), out)
    }

    fn options(out: &Path) -> FetchOptions {
        FetchOptions {
            repository: "acme/ledger".to_owned(),
            out: out.to_path_buf(),
            issues: 1,
            discussions: 0,
            pulls: 0,
        }
    }

    #[test]
    fn refresh_periods_are_floored_and_never_overflow() {
        assert_eq!(refresh_period(1), Duration::from_secs(300));
        assert_eq!(refresh_period(5), Duration::from_secs(300));
        assert_eq!(refresh_period(60), Duration::from_secs(3600));
        assert_eq!(refresh_period(u64::MAX), Duration::from_secs(u64::MAX));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_that_wrote_files_rescans_only_the_sources_under_its_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, out) = engine_with_export_source(dir.path());
        let report = refresh_github_once(&engine, &options(&out), |opts| {
            assert_eq!(opts.repository, "acme/ledger");
            Ok(FetchReport {
                issues: 2,
                ..FetchReport::default()
            })
        })
        .await
        .expect("refresh");
        assert_eq!(report.issues, 2);
        let pages = engine
            .store()
            .nodes_by_kind(crate::base::NodeKind::WikiPage, 10, 0)
            .expect("pages");
        assert_eq!(pages.len(), 1, "only the exported source was scanned");
        assert!(pages[0].name.contains("First"), "{}", pages[0].name);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_that_changed_nothing_does_not_rescan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, out) = engine_with_export_source(dir.path());
        let report = refresh_github_once(&engine, &options(&out), |_| {
            Ok(FetchReport {
                skipped: 5,
                ..FetchReport::default()
            })
        })
        .await
        .expect("refresh");
        assert_eq!(report.skipped, 5);
        assert_eq!(engine.store().stats().expect("stats").node_count, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_fetch_surfaces_and_leaves_the_graph_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, out) = engine_with_export_source(dir.path());
        let err = refresh_github_once(&engine, &options(&out), |_| {
            Err(Error::other("gh: not logged in"))
        })
        .await
        .expect_err("fetch failure");
        assert!(err.to_string().contains("not logged in"), "{err}");
        assert_eq!(engine.store().stats().expect("stats").node_count, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_fetch_is_an_error_not_a_dead_refresher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, out) = engine_with_export_source(dir.path());
        let result = refresh_github_once(&engine, &options(&out), |_| panic!("boom")).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rescan_under_reports_which_sources_it_scanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, out) = engine_with_export_source(dir.path());
        assert_eq!(
            rescan_under(&engine, &out).await.expect("scan"),
            ["exported"]
        );
        assert!(
            rescan_under(&engine, &dir.path().join("nowhere"))
                .await
                .expect("scan")
                .is_empty()
        );
    }

    fn changed() -> EngineEvent {
        EngineEvent::GraphChanged {
            nodes_upserted: 1,
            nodes_removed: 0,
            edges_upserted: 0,
            edges_removed: 0,
        }
    }

    #[tokio::test]
    async fn the_embed_loop_runs_once_per_graph_change_and_stops_when_the_channel_closes() {
        let (sender, receiver) = broadcast::channel(16);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let task = tokio::spawn(run_embed_loop(receiver, Duration::ZERO, move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }));
        sender.send(changed()).expect("send");
        sender
            .send(EngineEvent::FileIngested {
                source: "s".to_owned(),
                path: "p".to_owned(),
                latency_us: 1,
            })
            .expect("send");
        sender.send(changed()).expect("send");
        drop(sender);
        task.await.expect("the loop ends once the sender is gone");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "two graph changes, the file event ignored"
        );
    }

    #[tokio::test]
    async fn the_embed_loop_survives_lagging_behind_the_event_stream() {
        let (sender, receiver) = broadcast::channel(2);
        for _ in 0..10 {
            sender.send(changed()).expect("send");
        }
        drop(sender);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        run_embed_loop(receiver, Duration::ZERO, move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        // Only the two retained events are seen; the lag error is skipped, not fatal.
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embed_and_enrich_embeds_new_chunks_after_a_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _) = engine_with_export_source(dir.path());
        assert_eq!(engine.vectors().len(), 0);
        engine.scan_all().expect("scan");
        embed_and_enrich(&engine).await;
        assert!(!engine.vectors().is_empty(), "pending chunks were embedded");
        // A second pass has nothing to do and must not fail.
        embed_and_enrich(&engine).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_spawned_loop_embeds_after_a_graph_change_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _) = engine_with_export_source(dir.path());
        let task = spawn_embed_loop(&engine);
        let mut events = engine.subscribe();
        let scanner = engine.clone();
        tokio::task::spawn_blocking(move || scanner.scan_all())
            .await
            .expect("join")
            .expect("scan");
        // The scan published GraphChanged; wait for it, then for the debounced pass.
        while !matches!(events.recv().await, Ok(EngineEvent::GraphChanged { .. })) {}
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while engine.vectors().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!engine.vectors().is_empty());
        task.abort();
    }
}
