//! The background worker that expands the knowledge graph for open backlog items.
//!
//! The worker sleeps on a [`tokio::sync::Notify`] (signalled whenever an item is recorded), a
//! timer (the configured interval, shortened to the moment the earliest backed-off item becomes
//! claimable) and a cancellation token; it never polls, so an idle worker uses no CPU. It claims
//! items one at a time and runs the [`GapExpander`] in a separate task, so a panicking expander
//! fails only its item.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::base::Result;

use super::config::BacklogConfig;
use super::store::{Backlog, BacklogItem};

/// What an expander achieved for one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpansionOutcome {
    /// The gap is filled; the item is resolved with this note.
    Resolved {
        /// What was done.
        note: String,
    },
    /// Expansion ran but could not fill the gap; the item returns to open with a backoff and is
    /// dismissed after too many such attempts.
    Unresolved {
        /// Why not.
        reason: String,
    },
    /// Nothing could be done right now (offline, over budget); try again later with a backoff.
    Retry {
        /// Why later.
        reason: String,
    },
}

/// Fills a gap: fetches or derives the missing context. Implemented by the query engine.
#[async_trait]
pub trait GapExpander: Send + Sync + 'static {
    /// Tries to expand the context around `item`.
    async fn expand(&self, item: &BacklogItem) -> ExpansionOutcome;
}

/// The worker; see the module documentation.
pub struct BacklogWorker {
    backlog: Backlog,
    expander: Arc<dyn GapExpander>,
    interval: Duration,
    enabled: bool,
}

/// Handle to a spawned [`BacklogWorker`].
pub struct WorkerHandle {
    cancel: CancellationToken,
    join: JoinHandle<()>,
    processed: Arc<AtomicU64>,
    wakeups: Arc<AtomicU64>,
}

impl WorkerHandle {
    /// Items the worker has finished with (any outcome).
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }

    /// Number of passes the worker has made over the backlog (a measure of idle wake-ups).
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    /// A token that stops the worker when cancelled.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stops the worker (an expansion in flight is abandoned and its item released) and waits for
    /// it to finish.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        if let Err(err) = self.join.await {
            tracing::warn!(error = %err, "backlog worker ended abnormally");
        }
    }
}

/// Runs a blocking backlog operation on the blocking pool.
async fn blocking<T: Send + 'static>(
    backlog: &Backlog,
    f: impl FnOnce(&Backlog) -> Result<T> + Send + 'static,
) -> Result<T> {
    let backlog = backlog.clone();
    tokio::task::spawn_blocking(move || f(&backlog)).await?
}

impl BacklogWorker {
    /// A worker over `backlog` using the interval and enabled flag of `config`.
    pub fn new(backlog: Backlog, expander: Arc<dyn GapExpander>, config: &BacklogConfig) -> Self {
        Self {
            backlog,
            expander,
            interval: Duration::from_secs(config.worker_interval_secs.max(1)),
            enabled: config.enabled,
        }
    }

    /// Overrides the idle interval (tests use short ones).
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval.max(Duration::from_millis(1));
        self
    }

    /// Spawns the worker on the current Tokio runtime. When the backlog is disabled in the
    /// configuration the returned handle belongs to a task that does nothing.
    pub fn spawn(self) -> WorkerHandle {
        let cancel = CancellationToken::new();
        let processed = Arc::new(AtomicU64::new(0));
        let wakeups = Arc::new(AtomicU64::new(0));
        let join = if self.enabled {
            tokio::spawn(self.run_counted(
                cancel.clone(),
                Arc::clone(&processed),
                Arc::clone(&wakeups),
            ))
        } else {
            tokio::spawn(async {})
        };
        WorkerHandle {
            cancel,
            join,
            processed,
            wakeups,
        }
    }

    /// Runs until `cancel` is cancelled.
    pub async fn run(self, cancel: CancellationToken) {
        self.run_counted(
            cancel,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
        .await;
    }

    async fn run_counted(
        self,
        cancel: CancellationToken,
        processed: Arc<AtomicU64>,
        wakeups: Arc<AtomicU64>,
    ) {
        match blocking(&self.backlog, Backlog::recover_interrupted).await {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "recovered interrupted backlog items"),
            Err(err) => tracing::warn!(error = %err, "backlog recovery failed"),
        }
        let notify = self.backlog.notifier();
        loop {
            wakeups.fetch_add(1, Ordering::Relaxed);
            // Drain everything claimable.
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                match blocking(&self.backlog, Backlog::claim_next).await {
                    Ok(Some(item)) => {
                        if !self.process(item, &cancel).await {
                            return;
                        }
                        processed.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(error = %err, "backlog claim failed");
                        break;
                    }
                }
            }
            let wait = match blocking(&self.backlog, |b| b.next_ready_in()).await {
                Ok(Some(ready_in)) => ready_in.max(Duration::from_millis(1)).min(self.interval),
                _ => self.interval,
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = notify.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Expands one claimed item. Returns `false` when the worker was cancelled meanwhile.
    async fn process(&self, item: BacklogItem, cancel: &CancellationToken) -> bool {
        let id = item.id;
        let expander = Arc::clone(&self.expander);
        let mut task = tokio::spawn(async move { expander.expand(&item).await });
        let joined = tokio::select! {
            joined = &mut task => joined,
            () = cancel.cancelled() => {
                task.abort();
                if let Err(err) = blocking(&self.backlog, move |b| b.release(id)).await {
                    tracing::warn!(error = %err, id, "could not release backlog item");
                }
                return false;
            }
        };
        let stored = match joined {
            Ok(ExpansionOutcome::Resolved { note }) => {
                blocking(&self.backlog, move |b| b.resolve(id, &note)).await
            }
            Ok(ExpansionOutcome::Unresolved { reason }) => {
                blocking(&self.backlog, move |b| b.fail(id, &reason).map(|_| ())).await
            }
            Ok(ExpansionOutcome::Retry { reason }) => {
                blocking(&self.backlog, move |b| b.defer(id, &reason)).await
            }
            Err(err) => {
                let reason = if err.is_panic() {
                    "expander panicked".to_owned()
                } else {
                    format!("expander task failed: {err}")
                };
                tracing::warn!(id, %reason, "backlog expansion failed");
                blocking(&self.backlog, move |b| b.fail(id, &reason).map(|_| ())).await
            }
        };
        if let Err(err) = stored {
            tracing::warn!(error = %err, id, "could not store backlog outcome");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Instant;

    use parking_lot::Mutex;

    use super::*;
    use crate::backlog::{GapReport, GapStatus};
    use crate::base::{Db, MigrationSet};

    const SCHEMA: &[MigrationSet] = &[MigrationSet {
        module: "backlog",
        migrations: crate::backlog::MIGRATIONS,
    }];

    /// Plays back scripted behaviours, then resolves everything.
    #[derive(Default)]
    struct Fake {
        script: Mutex<VecDeque<Behaviour>>,
        calls: AtomicU64,
        seen: Mutex<Vec<i64>>,
    }

    enum Behaviour {
        Outcome(ExpansionOutcome),
        Panic,
        Hang,
    }

    #[async_trait]
    impl GapExpander for Fake {
        async fn expand(&self, item: &BacklogItem) -> ExpansionOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().push(item.id);
            let next = self.script.lock().pop_front();
            match next {
                Some(Behaviour::Outcome(outcome)) => outcome,
                Some(Behaviour::Panic) => panic!("expander exploded"),
                Some(Behaviour::Hang) => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                None => ExpansionOutcome::Resolved { note: "ok".into() },
            }
        }
    }

    fn setup(script: Vec<Behaviour>) -> (Backlog, Arc<Fake>) {
        let db = Db::open_in_memory_with(SCHEMA).expect("db");
        let backlog = Backlog::new(db, &BacklogConfig::default())
            .with_backoff(Duration::from_millis(5), Duration::from_millis(20))
            .with_max_attempts(3);
        let fake = Arc::new(Fake {
            script: Mutex::new(script.into()),
            ..Fake::default()
        });
        (backlog, fake)
    }

    fn worker(backlog: &Backlog, fake: &Arc<Fake>, interval: Duration) -> WorkerHandle {
        let expander: Arc<dyn GapExpander> = fake.clone();
        BacklogWorker::new(backlog.clone(), expander, &BacklogConfig::default())
            .with_interval(interval)
            .spawn()
    }

    fn gap(n: u32) -> GapReport {
        GapReport::MissingNeighbor {
            node_key: format!("api:{n}"),
            relation: "owned_by".into(),
            note: None,
        }
    }

    async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn status(backlog: &Backlog, id: i64) -> GapStatus {
        backlog.get(id).unwrap().unwrap().status
    }

    #[tokio::test]
    async fn processes_a_new_item_promptly_despite_a_long_interval() {
        let (backlog, fake) = setup(vec![]);
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        let id = backlog.record(&gap(1)).unwrap();
        wait_for("resolved", || status(&backlog, id) == GapStatus::Resolved).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        let it = backlog.get(id).unwrap().unwrap();
        assert_eq!(it.resolution_note.as_deref(), Some("ok"));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn drains_items_present_at_start_best_first() {
        let (backlog, fake) = setup(vec![]);
        let minor = backlog
            .record(&GapReport::UnresolvedEntity {
                mention: "x".into(),
                query: None,
            })
            .unwrap();
        let major = backlog.record(&gap(1)).unwrap();
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("both resolved", || handle.processed() == 2).await;
        assert_eq!(*fake.seen.lock(), vec![major, minor]);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn retry_outcomes_come_back_after_the_backoff() {
        let (backlog, fake) = setup(vec![
            Behaviour::Outcome(ExpansionOutcome::Retry {
                reason: "offline".into(),
            }),
            Behaviour::Outcome(ExpansionOutcome::Unresolved {
                reason: "nothing found".into(),
            }),
        ]);
        let id = backlog.record(&gap(1)).unwrap();
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("resolved on third try", || {
            status(&backlog, id) == GapStatus::Resolved
        })
        .await;
        let it = backlog.get(id).unwrap().unwrap();
        assert_eq!(it.attempts, 3);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn persistent_failure_ends_dismissed() {
        let script = (0..10)
            .map(|_| {
                Behaviour::Outcome(ExpansionOutcome::Unresolved {
                    reason: "no luck".into(),
                })
            })
            .collect();
        let (backlog, fake) = setup(script);
        let id = backlog.record(&gap(1)).unwrap();
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("dismissed", || status(&backlog, id) == GapStatus::Dismissed).await;
        assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_panicking_expander_does_not_kill_the_worker() {
        let (backlog, fake) = setup(vec![Behaviour::Panic]);
        let first = backlog.record(&gap(1)).unwrap();
        let second = backlog.record(&gap(2)).unwrap();
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("both resolved", || {
            status(&backlog, first) == GapStatus::Resolved
                && status(&backlog, second) == GapStatus::Resolved
        })
        .await;
        // The panicked item recorded its failure.
        let failed = [first, second]
            .into_iter()
            .filter_map(|id| backlog.get(id).unwrap())
            .any(|it| it.last_error.as_deref() == Some("expander panicked"));
        assert!(failed);
        // The worker is still alive and serves later items.
        let third = backlog.record(&gap(3)).unwrap();
        wait_for("third resolved", || {
            status(&backlog, third) == GapStatus::Resolved
        })
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn idle_worker_does_not_wake_up() {
        let (backlog, fake) = setup(vec![]);
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(handle.processed(), 0);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        // One initial pass, nothing since.
        assert!(handle.wakeups() <= 1, "wakeups: {}", handle.wakeups());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_prompt_and_releases_the_item_in_flight() {
        let (backlog, fake) = setup(vec![Behaviour::Hang]);
        let id = backlog.record(&gap(1)).unwrap();
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("expansion started", || {
            fake.calls.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(status(&backlog, id), GapStatus::InProgress);
        let started = Instant::now();
        handle.shutdown().await;
        assert!(started.elapsed() < Duration::from_secs(2));
        let it = backlog.get(id).unwrap().unwrap();
        assert_eq!((it.status, it.attempts), (GapStatus::Open, 0));
    }

    #[tokio::test]
    async fn startup_recovers_items_left_in_progress() {
        let (backlog, fake) = setup(vec![]);
        let id = backlog.record(&gap(1)).unwrap();
        backlog.claim_next().unwrap().unwrap(); // the "crashed" process
        assert_eq!(status(&backlog, id), GapStatus::InProgress);
        let handle = worker(&backlog, &fake, Duration::from_secs(3600));
        wait_for("resolved", || status(&backlog, id) == GapStatus::Resolved).await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_backlog_runs_nothing() {
        let (backlog, fake) = setup(vec![]);
        let id = backlog.record(&gap(1)).unwrap();
        let expander: Arc<dyn GapExpander> = fake.clone();
        let config = BacklogConfig {
            enabled: false,
            ..BacklogConfig::default()
        };
        let handle = BacklogWorker::new(backlog.clone(), expander, &config).spawn();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(status(&backlog, id), GapStatus::Open);
        handle.shutdown().await;
    }
}
