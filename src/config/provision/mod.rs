//! Ollama provisioning: find, install, start, fill and (for tests and recovery) wipe and rebuild a
//! local Ollama backend without root privileges.
//!
//! Everything Anchor installs lives in one *managed root* ([`AnchorPaths::ollama_dir`]):
//!
//! ```text
//! <root>/.anchor-managed   marker; nothing is ever deleted from a directory without it
//! <root>/dist/             extracted official build (bin/ollama, lib/ollama/...)
//! <root>/models/           OLLAMA_MODELS of the managed server
//! <root>/home/             HOME of the managed server (its key pair; never ~/.ollama)
//! <root>/logs/ollama.log   stdout and stderr of the managed server
//! <root>/cache/            downloaded archives (+ checksum metadata)
//! <root>/state.json        install manifest, server pid/port, models pulled by Anchor
//! <root>/lock              inter-process lock serialising every operation
//! ```
//!
//! [`OllamaManager`] is the entry point: [`status`](OllamaManager::status),
//! [`ensure_ready`](OllamaManager::ensure_ready), [`stop`](OllamaManager::stop),
//! [`teardown`](OllamaManager::teardown), [`rebuild`](OllamaManager::rebuild) and
//! [`verify`](OllamaManager::verify). Process launching and binary discovery go through the
//! [`ProcessControl`] and [`BinaryLocator`] traits; [`testing`] provides in-process fakes.
//!
//! An Ollama server that is already reachable at the configured URL is *external*: it is used,
//! never installed, stopped or wiped. Only the managed server is ever signalled, and only after its
//! pid has been verified to be the recorded executable running `serve`.

mod guard;
mod install;
mod process;
#[doc(hidden)]
pub mod testing;
crate::base::opaque_debug!(OllamaManager);

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

pub use guard::{
    BinarySource, InstallManifest, MARKER_FILE, ManagedLayout, ManagedState, PulledModel,
    RemovedPath, ServerRecord,
};
pub use install::{DEFAULT_DOWNLOAD_BASE_URL, artifact_stem};
pub use process::{
    BinaryLocator, LaunchSpec, PathLocator, ProcessControl, StopSignal, SystemProcesses,
};

use guard::{RootLock, check_root, io_error, prepare_root, remove_inside};
use install::{InstallRequest, install};

use super::{AnchorPaths, OllamaConfig};
use crate::base::time::now_ms;
use crate::ollama::{GenerateRequest, OllamaApi, OllamaError, PullProgress, normalize_base_url};

/// Everything that can go wrong while provisioning. Every variant says what failed and where.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// A path was refused because deleting from or writing to it would be unsafe.
    #[error("refusing to use {}: {reason}", path.display())]
    UnsafePath {
        /// The offending path.
        path: PathBuf,
        /// Why it was refused.
        reason: String,
    },
    /// Another process holds the managed-root lock.
    #[error("timed out after {waited_ms} ms waiting for another Anchor process to release {}", path.display())]
    LockTimeout {
        /// The lock file.
        path: PathBuf,
        /// How long was waited.
        waited_ms: u64,
    },
    /// No official build exists for this platform.
    #[error(
        "Anchor cannot install Ollama on {os}/{arch}; install it manually and set ollama.base_url"
    )]
    UnsupportedPlatform {
        /// Operating system.
        os: String,
        /// CPU architecture.
        arch: String,
    },
    /// Downloading the archive failed.
    #[error("download from {url} failed: {reason}")]
    Download {
        /// URL requested.
        url: String,
        /// What went wrong.
        reason: String,
    },
    /// The archive's SHA-256 differs from the published one.
    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// Archive name.
        file: String,
        /// Published checksum.
        expected: String,
        /// Computed checksum.
        actual: String,
    },
    /// The archive cannot be decompressed or unpacked, or lacks `bin/ollama`.
    #[error("invalid archive {}: {reason}", archive.display())]
    InvalidArchive {
        /// The archive.
        archive: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The archive contains an entry that would escape the installation directory.
    #[error("unsafe archive entry `{entry}`: {reason}")]
    UnsafeArchiveEntry {
        /// Entry path as stored in the archive.
        entry: String,
        /// Why it was refused.
        reason: String,
    },
    /// A call to the Ollama API failed.
    #[error("{context}: {source}")]
    Api {
        /// What was being done.
        context: String,
        /// The API error (carries the remote error message for failed pulls).
        source: OllamaError,
    },
    /// The server process could not be started or died right after starting.
    #[error("cannot start the Ollama server: {0}")]
    StartFailed(String),
    /// The server was started but never answered.
    #[error(
        "the Ollama server did not become healthy at {base_url} within {secs} s; log tail:\n{log_tail}"
    )]
    HealthTimeout {
        /// URL probed.
        base_url: String,
        /// Time waited.
        secs: u64,
        /// Last lines of the server log.
        log_tail: String,
    },
    /// The configured server is not reachable and provisioning is not possible.
    #[error("no Ollama server is reachable at {0} and none is managed by Anchor")]
    NoServer(String),
    /// The smoke-test generation failed.
    #[error("generation check with `{model}` failed: {reason}")]
    VerifyFailed {
        /// Model used.
        model: String,
        /// What went wrong.
        reason: String,
    },
    /// The configured base URL is not usable.
    #[error("invalid Ollama URL `{0}`")]
    InvalidUrl(String),
}

impl From<ProvisionError> for crate::base::Error {
    fn from(err: ProvisionError) -> Self {
        crate::base::Error::module("ollama-provision", err)
    }
}

/// Progress of a provisioning step, for the caller's progress bar or log.
#[derive(Clone, Debug, PartialEq)]
pub enum ProvisionEvent {
    /// A phase started or finished; human-readable.
    Step(String),
    /// Archive download progress.
    Download {
        /// Archive name.
        file: String,
        /// Bytes received.
        received: u64,
        /// Total bytes, when the server announced them.
        total: Option<u64>,
    },
    /// Archive extraction progress.
    Extract {
        /// Entries unpacked so far.
        entries: u64,
    },
    /// Model pull progress.
    Pull {
        /// Model being pulled.
        model: String,
        /// Server status line (`pulling manifest`, `pulling <digest>`, `success`).
        status: String,
        /// Bytes of the current layer received.
        completed: Option<u64>,
        /// Size of the current layer.
        total: Option<u64>,
    },
}

/// Receiver of [`ProvisionEvent`]s.
pub type ProgressFn<'a> = &'a (dyn Fn(ProvisionEvent) + Send + Sync);

/// A progress callback that ignores everything.
pub fn no_progress(_event: ProvisionEvent) {}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Knobs of [`OllamaManager::ensure_ready`].
#[derive(Clone, Debug)]
pub struct ProvisionOptions {
    /// Model that must be present; `None` uses `OllamaConfig::default_model`.
    pub model: Option<String>,
    /// Base URL of the release downloads (`<base>/ollama-linux-amd64.tar.zst`, ...).
    pub download_base_url: String,
    /// Pinned release (`0.5.7`); `None` installs the latest.
    pub version: Option<String>,
    /// Expected archive SHA-256 (lowercase hex). Overrides the published checksum.
    pub sha256: Option<String>,
    /// Port tried first for a managed server; `None` derives it from the configured URL (its port
    /// when loopback, else 11434). Another free port is used when it is taken.
    pub preferred_port: Option<u16>,
    /// Use an `ollama` found on `PATH` instead of downloading one.
    pub use_system_binary: bool,
    /// Run the smoke-test generation after the model is present.
    pub verify: bool,
    /// How long a freshly started server may take to answer; `None` uses the configuration.
    pub startup_timeout: Option<Duration>,
    /// Deadline of one liveness probe of the configured endpoint.
    pub probe_timeout: Duration,
    /// How long to wait for the managed-root lock.
    pub lock_timeout: Duration,
    /// Deadline of the smoke-test generation (the first call loads the model).
    pub verify_timeout: Duration,
}

impl Default for ProvisionOptions {
    fn default() -> Self {
        Self {
            model: None,
            download_base_url: DEFAULT_DOWNLOAD_BASE_URL.to_owned(),
            version: None,
            sha256: None,
            preferred_port: None,
            use_system_binary: true,
            verify: true,
            startup_timeout: None,
            probe_timeout: Duration::from_secs(2),
            lock_timeout: Duration::from_secs(15 * 60),
            verify_timeout: Duration::from_secs(300),
        }
    }
}

/// Knobs of [`OllamaManager::teardown`].
#[derive(Clone, Debug)]
pub struct TeardownOptions {
    /// Keep the installed distribution (and its manifest).
    pub keep_distribution: bool,
    /// Keep the download cache (`false` removes it).
    pub keep_cache: bool,
    /// Delete, through the API, the models Anchor pulled on an *external* server. Off by default:
    /// an external server is otherwise left completely alone.
    pub remove_external_models: bool,
    /// How long to wait for the server to exit after `SIGTERM` before `SIGKILL`.
    pub stop_timeout: Duration,
    /// How long to wait for the managed-root lock.
    pub lock_timeout: Duration,
}

impl Default for TeardownOptions {
    fn default() -> Self {
        Self {
            keep_distribution: false,
            keep_cache: false,
            remove_external_models: false,
            stop_timeout: Duration::from_secs(15),
            lock_timeout: Duration::from_secs(15 * 60),
        }
    }
}

/// Knobs of [`OllamaManager::rebuild`].
#[derive(Clone, Debug, Default)]
pub struct RebuildOptions {
    /// How to wipe.
    pub teardown: TeardownOptions,
    /// How to provision afterwards.
    pub provision: ProvisionOptions,
}

/// Which kind of server serves the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// A server Anchor does not manage (never installed, stopped or wiped).
    External,
    /// The managed server was already running.
    ManagedRunning,
    /// The managed server was started by this call.
    ManagedStarted,
}

/// Time spent in one phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepTiming {
    /// Phase name (`probe`, `install`, `start`, `pull`, `verify`, ...).
    pub step: String,
    /// Duration in milliseconds.
    pub ms: u64,
}

/// Result of the smoke-test generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    /// Model used.
    pub model: String,
    /// Server used.
    pub base_url: String,
    /// Wall-clock latency of the generation.
    pub latency_ms: u64,
    /// Prompt tokens as reported by the server.
    pub prompt_tokens: u64,
    /// Generated tokens as reported by the server.
    pub completion_tokens: u64,
    /// The (short) generated text.
    pub response: String,
}

/// What [`OllamaManager::ensure_ready`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProvisionReport {
    /// Which server is used.
    pub mode: ServerMode,
    /// The effective base URL; the caller persists it into the configuration.
    pub base_url: String,
    /// Server version.
    pub version: Option<String>,
    /// Executable of the managed server.
    pub binary: Option<PathBuf>,
    /// Where that executable came from.
    pub binary_source: Option<BinarySource>,
    /// The distribution was downloaded and installed by this call.
    pub installed: bool,
    /// The archive was downloaded (`false` when it came from the cache).
    pub downloaded: bool,
    /// The managed server was started by this call.
    pub started: bool,
    /// Model made available.
    pub model: String,
    /// The model had to be pulled by this call.
    pub pulled: bool,
    /// Smoke-test result, when requested.
    pub verification: Option<VerifyReport>,
    /// Per-step timings.
    pub steps: Vec<StepTiming>,
    /// Total duration.
    pub total_ms: u64,
}

/// What [`OllamaManager::teardown`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TeardownReport {
    /// A managed server was running and was stopped.
    pub stopped_server: bool,
    /// Everything deleted, with sizes.
    pub removed: Vec<RemovedPath>,
    /// Total bytes freed.
    pub bytes_freed: u64,
    /// Models deleted from an external server through the API.
    pub external_models_deleted: Vec<String>,
    /// External-server models that could not be deleted, with the reason.
    pub external_models_failed: Vec<(String, String)>,
    /// The (now empty) managed root itself was removed.
    pub root_removed: bool,
}

/// What [`OllamaManager::rebuild`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RebuildReport {
    /// The wipe.
    pub teardown: TeardownReport,
    /// Duration of the wipe.
    pub teardown_ms: u64,
    /// The re-provisioning.
    pub provision: ProvisionReport,
    /// Duration of the re-provisioning.
    pub provision_ms: u64,
    /// The final smoke test.
    pub verification: VerifyReport,
    /// Duration of the smoke test.
    pub verify_ms: u64,
    /// Total duration.
    pub total_ms: u64,
}

/// Reachability of an Ollama endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EndpointStatus {
    /// The URL probed.
    pub base_url: String,
    /// Server version when it answered.
    pub version: Option<String>,
    /// Models it holds (empty when unreachable).
    pub models: Vec<String>,
}

impl EndpointStatus {
    /// `true` when the endpoint answered.
    pub fn reachable(&self) -> bool {
        self.version.is_some()
    }
}

/// Snapshot returned by [`OllamaManager::status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OllamaStatus {
    /// The configured endpoint.
    pub endpoint: EndpointStatus,
    /// The managed root.
    pub root: PathBuf,
    /// The root exists and carries the Anchor marker.
    pub root_marked: bool,
    /// The install manifest, when a distribution is installed.
    pub install: Option<InstallManifest>,
    /// The managed distribution's executable exists.
    pub distribution_present: bool,
    /// The recorded managed server, if any.
    pub server: Option<ServerRecord>,
    /// That server's process is alive and verified to be the recorded executable.
    pub server_alive: bool,
    /// Its API answers (probed at its own URL).
    pub server_endpoint: Option<EndpointStatus>,
    /// Models Anchor pulled.
    pub pulled_models: Vec<PulledModel>,
    /// An `ollama` executable on `PATH`.
    pub system_binary: Option<PathBuf>,
    /// The state file was unreadable and has been ignored.
    pub state_recovered: bool,
}

/// Provisions and controls the Ollama backend. See the module documentation.
pub struct OllamaManager {
    config: OllamaConfig,
    layout: ManagedLayout,
    processes: Arc<dyn ProcessControl>,
    locator: Arc<dyn BinaryLocator>,
    home: Option<PathBuf>,
}

struct Timings {
    started: Instant,
    steps: Vec<StepTiming>,
}

impl Timings {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            steps: Vec::new(),
        }
    }

    fn record(&mut self, step: &str, since: Instant) {
        self.steps.push(StepTiming {
            step: step.to_owned(),
            ms: elapsed_ms(since),
        });
    }
}

/// Lowercase, loopback-aware comparison of two base URLs.
fn same_endpoint(a: &str, b: &str) -> bool {
    match (normalize_base_url(a), normalize_base_url(b)) {
        (Ok(a), Ok(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn api_error(context: impl Into<String>, source: OllamaError) -> ProvisionError {
    ProvisionError::Api {
        context: context.into(),
        source,
    }
}

impl OllamaManager {
    /// Manager for the real system: the managed root is `paths.ollama_dir()`, processes are real
    /// and `ollama` is looked up on `PATH`.
    pub fn new(config: OllamaConfig, paths: &AnchorPaths) -> Self {
        Self::with_root(config, paths.ollama_dir())
    }

    /// Like [`Self::new`] with an explicit managed root.
    pub fn with_root(config: OllamaConfig, root: impl Into<PathBuf>) -> Self {
        Self::with_parts(
            config,
            root,
            Arc::new(SystemProcesses),
            Arc::new(PathLocator),
        )
    }

    /// Manager with injected process control and binary discovery (tests, other platforms).
    pub fn with_parts(
        config: OllamaConfig,
        root: impl Into<PathBuf>,
        processes: Arc<dyn ProcessControl>,
        locator: Arc<dyn BinaryLocator>,
    ) -> Self {
        Self {
            config,
            layout: ManagedLayout::new(root),
            processes,
            locator,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// Overrides the home directory the deletion guard protects (default: `$HOME`).
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// The managed layout.
    pub fn layout(&self) -> &ManagedLayout {
        &self.layout
    }

    /// The configuration this manager was built with.
    pub fn config(&self) -> &OllamaConfig {
        &self.config
    }

    fn api(&self, base_url: &str, probe: Duration) -> Result<OllamaApi, ProvisionError> {
        OllamaApi::new(base_url)
            .map(|api| api.with_probe_timeout(probe))
            .map_err(|_| ProvisionError::InvalidUrl(base_url.to_owned()))
    }

    async fn endpoint_status(&self, base_url: &str, probe: Duration) -> EndpointStatus {
        let mut status = EndpointStatus {
            base_url: base_url.to_owned(),
            version: None,
            models: Vec::new(),
        };
        let Ok(api) = self.api(base_url, probe) else {
            return status;
        };
        status.version = api.version().await.ok();
        if status.version.is_some() {
            status.models = api
                .list_models()
                .await
                .map(|models| models.into_iter().map(|m| m.name).collect())
                .unwrap_or_default();
        }
        status
    }

    /// Reports the state of the endpoint, the managed installation and the system, without
    /// changing anything.
    pub async fn status(&self) -> OllamaStatus {
        let probe = ProvisionOptions::default().probe_timeout;
        let (state, state_recovered) = ManagedState::load(&self.layout);
        let endpoint = self.endpoint_status(&self.config.base_url, probe).await;
        let server_alive = state
            .server
            .as_ref()
            .is_some_and(|server| self.processes.is_running(server.pid, &server.binary));
        let server_endpoint = match &state.server {
            Some(server) if server_alive => {
                Some(self.endpoint_status(&server.base_url, probe).await)
            }
            _ => None,
        };
        OllamaStatus {
            endpoint,
            root: self.layout.root().to_path_buf(),
            root_marked: self.layout.is_marked(),
            distribution_present: self.layout.binary().is_file(),
            install: state.install,
            server: state.server,
            server_alive,
            server_endpoint,
            pulled_models: state.models,
            system_binary: self.locator.system_ollama(),
            state_recovered,
        }
    }

    /// The managed server recorded in `state`, when its process is alive.
    fn live_server(&self, state: &ManagedState) -> Option<ServerRecord> {
        state
            .server
            .clone()
            .filter(|server| self.processes.is_running(server.pid, &server.binary))
    }

    /// Makes a server with the wanted model available and reports what it took. See the module
    /// documentation for the decision order. Idempotent: a second call does nothing but probe.
    pub async fn ensure_ready(
        &self,
        options: &ProvisionOptions,
        progress: ProgressFn<'_>,
    ) -> Result<ProvisionReport, ProvisionError> {
        let mut timings = Timings::new();
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| self.config.default_model.clone());
        prepare_root(&self.layout)?;
        let _lock = RootLock::acquire(&self.layout.lock_file(), options.lock_timeout).await?;
        let (mut state, recovered) = ManagedState::load(&self.layout);
        if recovered {
            progress(ProvisionEvent::Step(
                "recovered from an unreadable state file".to_owned(),
            ));
        }

        let probe_started = Instant::now();
        let live = self.live_server(&state);
        if state.server.is_some() && live.is_none() {
            progress(ProvisionEvent::Step(
                "forgetting a stale server record".to_owned(),
            ));
            state.server = None;
            state.save(&self.layout)?;
        }
        let configured = self
            .endpoint_status(&self.config.base_url, options.probe_timeout)
            .await;
        timings.record("probe", probe_started);

        let mut report = ProvisionReport {
            mode: ServerMode::External,
            base_url: self.config.base_url.clone(),
            version: configured.version.clone(),
            binary: None,
            binary_source: None,
            installed: false,
            downloaded: false,
            started: false,
            model: model.clone(),
            pulled: false,
            verification: None,
            steps: Vec::new(),
            total_ms: 0,
        };

        if configured.reachable() {
            match &live {
                Some(server) if same_endpoint(&server.base_url, &self.config.base_url) => {
                    report.mode = ServerMode::ManagedRunning;
                    report.binary = Some(server.binary.clone());
                    report.binary_source = Some(server.binary_source);
                }
                _ => progress(ProvisionEvent::Step(format!(
                    "using the Ollama server at {} (not managed by Anchor)",
                    self.config.base_url
                ))),
            }
        } else if let Some(server) = live {
            let status = self
                .endpoint_status(&server.base_url, options.probe_timeout)
                .await;
            if status.reachable() {
                report.mode = ServerMode::ManagedRunning;
                report.base_url = server.base_url.clone();
                report.version = status.version;
                report.binary = Some(server.binary.clone());
                report.binary_source = Some(server.binary_source);
            } else {
                // Alive but not answering (e.g. still starting): wait for it instead of starting
                // a second one on the same models directory.
                let timeout = self.startup_timeout(options);
                report.version = self
                    .wait_healthy(
                        &server.base_url,
                        server.pid,
                        &server.binary,
                        timeout,
                        options,
                    )
                    .await?;
                report.mode = ServerMode::ManagedRunning;
                report.base_url = server.base_url.clone();
                report.binary = Some(server.binary.clone());
                report.binary_source = Some(server.binary_source);
            }
        } else {
            self.start_managed(options, progress, &mut state, &mut timings, &mut report)
                .await?;
        }

        // The model.
        let managed = report.mode != ServerMode::External;
        let api = self.api(&report.base_url, options.probe_timeout)?;
        let pull_started = Instant::now();
        let present = api
            .has_model(&model)
            .await
            .map_err(|err| api_error("listing models", err))?;
        if !present {
            progress(ProvisionEvent::Step(format!("pulling {model}")));
            self.pull_with_progress(&api, &model, progress).await?;
            state.models.retain(|pulled| {
                !(pulled.name == model && same_endpoint(&pulled.base_url, &report.base_url))
            });
            state.models.push(PulledModel {
                name: model.clone(),
                base_url: report.base_url.clone(),
                managed,
                pulled_at: now_ms(),
            });
            state.save(&self.layout)?;
            report.pulled = true;
        }
        timings.record("model", pull_started);

        if options.verify {
            let verify_started = Instant::now();
            progress(ProvisionEvent::Step(format!(
                "checking {model} with a short generation"
            )));
            report.verification = Some(
                self.verify_at(&report.base_url, &model, options.verify_timeout)
                    .await?,
            );
            timings.record("verify", verify_started);
        }
        report.steps = timings.steps;
        report.total_ms = elapsed_ms(timings.started);
        Ok(report)
    }

    fn startup_timeout(&self, options: &ProvisionOptions) -> Duration {
        options
            .startup_timeout
            .unwrap_or_else(|| Duration::from_secs(self.config.startup_timeout_secs.max(1)))
    }

    async fn pull_with_progress(
        &self,
        api: &OllamaApi,
        model: &str,
        progress: ProgressFn<'_>,
    ) -> Result<(), ProvisionError> {
        let mut last: (String, u64) = (String::new(), 0);
        api.pull(model, |event: &PullProgress| {
            let completed = event.completed.unwrap_or(0);
            let step = event.total.map_or(8 << 20, |total| (total / 100).max(1));
            let changed = event.status != last.0;
            if changed || completed.saturating_sub(last.1) >= step {
                last = (event.status.clone(), completed);
                progress(ProvisionEvent::Pull {
                    model: model.to_owned(),
                    status: event.status.clone(),
                    completed: event.completed,
                    total: event.total,
                });
            }
        })
        .await
        .map_err(|err| api_error(format!("pulling model `{model}`"), err))
    }

    /// Chooses a binary (installing one when needed), starts the server and waits until healthy.
    async fn start_managed(
        &self,
        options: &ProvisionOptions,
        progress: ProgressFn<'_>,
        state: &mut ManagedState,
        timings: &mut Timings,
        report: &mut ProvisionReport,
    ) -> Result<(), ProvisionError> {
        let installed = state.install.is_some() && self.layout.binary().is_file();
        let system = if options.use_system_binary && !installed {
            self.locator.system_ollama()
        } else {
            None
        };
        let (binary, source) = if installed {
            (self.layout.binary(), BinarySource::Managed)
        } else if let Some(system) = system {
            progress(ProvisionEvent::Step(format!(
                "using the system ollama at {}",
                system.display()
            )));
            (system, BinarySource::System)
        } else {
            let install_started = Instant::now();
            progress(ProvisionEvent::Step(
                "installing Ollama into the managed directory".to_owned(),
            ));
            let outcome = install(InstallRequest {
                layout: &self.layout,
                base_url: &options.download_base_url,
                version: options.version.as_deref(),
                sha256: options.sha256.as_deref(),
                progress,
            })
            .await?;
            timings.record("install", install_started);
            timings.steps.push(StepTiming {
                step: "install.download".to_owned(),
                ms: outcome.download_ms,
            });
            timings.steps.push(StepTiming {
                step: "install.extract".to_owned(),
                ms: outcome.extract_ms,
            });
            report.installed = true;
            report.downloaded = outcome.downloaded;
            state.install = Some(outcome.manifest);
            state.save(&self.layout)?;
            (self.layout.binary(), BinarySource::Managed)
        };

        let start_started = Instant::now();
        for dir in [
            self.layout.models_dir(),
            self.layout.home_dir(),
            self.layout.logs_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|err| io_error(format!("cannot create {}", dir.display()), err))?;
        }
        // A free port can be taken by another process between choosing it and binding it, so a
        // launch that fails with "address in use" is retried on a fresh port.
        let mut preferred = self.preferred_port(options);
        let mut attempts = 0;
        let (pid, port) = loop {
            let port = pick_port(preferred)?;
            let spec = LaunchSpec {
                binary: binary.clone(),
                host: "127.0.0.1".to_owned(),
                port,
                models_dir: self.layout.models_dir(),
                home_dir: self.layout.home_dir(),
                log_file: self.layout.log_file(),
                num_parallel: self.config.num_parallel,
                context_tokens: self.config.context_tokens,
            };
            progress(ProvisionEvent::Step(format!(
                "starting ollama serve on {}",
                spec.ollama_host()
            )));
            match self.processes.spawn(&spec).await {
                Ok(pid) => break (pid, port),
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && attempts < 3 => {
                    attempts += 1;
                    preferred = 0;
                }
                Err(err) => return Err(ProvisionError::StartFailed(err.to_string())),
            }
        };
        let base_url = format!("http://127.0.0.1:{port}");
        state.server = Some(ServerRecord {
            pid,
            port,
            base_url: base_url.clone(),
            binary: binary.clone(),
            binary_source: source,
            started_at: now_ms(),
            version: None,
        });
        state.save(&self.layout)?;

        let timeout = self.startup_timeout(options);
        let version = match self
            .wait_healthy(&base_url, pid, &binary, timeout, options)
            .await
        {
            Ok(version) => version,
            Err(err) => {
                // Do not leave a half-started server behind.
                self.stop_recorded(state, Duration::from_secs(5)).await;
                let _ = state.save(&self.layout);
                return Err(err);
            }
        };
        if let Some(server) = state.server.as_mut() {
            server.version = version.clone();
        }
        if let Some(install) = state.install.as_mut() {
            install.version = version.clone();
        }
        state.save(&self.layout)?;
        timings.record("start", start_started);

        report.mode = ServerMode::ManagedStarted;
        report.started = true;
        report.base_url = base_url;
        report.version = version;
        report.binary = Some(binary);
        report.binary_source = Some(source);
        Ok(())
    }

    fn preferred_port(&self, options: &ProvisionOptions) -> u16 {
        options.preferred_port.unwrap_or_else(|| {
            reqwest::Url::parse(&self.config.base_url)
                .ok()
                .filter(|url| {
                    url.host_str()
                        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]"))
                })
                .and_then(|url| url.port_or_known_default())
                .map_or(11434, |port| if port == 80 { 11434 } else { port })
        })
    }

    /// Polls the server until it answers, dies, or `timeout` passes.
    async fn wait_healthy(
        &self,
        base_url: &str,
        pid: u32,
        binary: &Path,
        timeout: Duration,
        options: &ProvisionOptions,
    ) -> Result<Option<String>, ProvisionError> {
        let api = self.api(base_url, options.probe_timeout.min(Duration::from_secs(2)))?;
        let started = Instant::now();
        loop {
            if let Ok(version) = api.version().await {
                return Ok(Some(version));
            }
            if !self.processes.is_running(pid, binary) {
                return Err(ProvisionError::StartFailed(format!(
                    "the server process exited right after starting; log tail:\n{}",
                    self.log_tail()
                )));
            }
            if started.elapsed() >= timeout {
                return Err(ProvisionError::HealthTimeout {
                    base_url: base_url.to_owned(),
                    secs: timeout.as_secs(),
                    log_tail: self.log_tail(),
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn log_tail(&self) -> String {
        let bytes = std::fs::read(self.layout.log_file()).unwrap_or_default();
        let start = bytes.len().saturating_sub(2000);
        String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
    }

    /// Stops the recorded managed server (verified first) and clears the record. Returns whether
    /// a live process was stopped.
    async fn stop_recorded(&self, state: &mut ManagedState, timeout: Duration) -> bool {
        let Some(server) = state.server.take() else {
            return false;
        };
        if !self.processes.is_running(server.pid, &server.binary) {
            return false;
        }
        if let Err(err) = self.processes.signal(server.pid, StopSignal::Terminate) {
            tracing::warn!(error = %err, "cannot signal the managed Ollama server");
        }
        let started = Instant::now();
        while self.processes.is_running(server.pid, &server.binary) {
            if started.elapsed() >= timeout {
                if let Err(err) = self.processes.signal(server.pid, StopSignal::Kill) {
                    tracing::warn!(error = %err, "cannot kill the managed Ollama server");
                }
                // SIGKILL cannot be ignored; give the kernel a moment to reap it.
                let kill_started = Instant::now();
                while self.processes.is_running(server.pid, &server.binary)
                    && kill_started.elapsed() < Duration::from_secs(5)
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Stops the managed server if one is running (an external server is never touched). Returns
    /// whether a process was stopped.
    pub async fn stop(&self, timeout: Duration) -> Result<bool, ProvisionError> {
        if !self.layout.is_marked() {
            return Ok(false);
        }
        let _lock = RootLock::acquire(&self.layout.lock_file(), Duration::from_secs(60)).await?;
        let (mut state, _) = ManagedState::load(&self.layout);
        let stopped = self.stop_recorded(&mut state, timeout).await;
        state.save(&self.layout)?;
        Ok(stopped)
    }

    /// Stops the managed server and deletes what Anchor installed, strictly inside the managed
    /// root. See the module documentation and [`TeardownOptions`].
    pub async fn teardown(
        &self,
        options: &TeardownOptions,
    ) -> Result<TeardownReport, ProvisionError> {
        let mut report = TeardownReport::default();
        if std::fs::symlink_metadata(self.layout.root()).is_err() {
            return Ok(report);
        }
        let root = check_root(self.layout.root(), self.home.as_deref())?;
        let layout = ManagedLayout::new(&root);
        let lock = RootLock::acquire(&layout.lock_file(), options.lock_timeout).await?;
        let (mut state, _) = ManagedState::load(&layout);

        report.stopped_server = self.stop_recorded(&mut state, options.stop_timeout).await;

        if options.remove_external_models {
            let mut kept = Vec::new();
            for pulled in std::mem::take(&mut state.models) {
                if pulled.managed {
                    continue; // lives inside the root; removed with the models directory
                }
                let outcome = match self.api(&pulled.base_url, Duration::from_secs(2)) {
                    Ok(api) => api
                        .delete_model(&pulled.name)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(err) => Err(err.to_string()),
                };
                match outcome {
                    Ok(()) => report.external_models_deleted.push(pulled.name),
                    Err(reason) => {
                        report
                            .external_models_failed
                            .push((pulled.name.clone(), reason));
                        kept.push(pulled);
                    }
                }
            }
            state.models = kept;
        } else {
            state.models.retain(|pulled| !pulled.managed);
        }

        let mut doomed = vec![layout.models_dir(), layout.home_dir(), layout.logs_dir()];
        if !options.keep_distribution {
            doomed.push(layout.dist_dir());
            state.install = None;
        }
        if !options.keep_cache {
            doomed.push(layout.cache_dir());
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            doomed.extend(
                entries
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("staging-"))
                    .map(|entry| entry.path()),
            );
        }
        state.server = None;
        state.save(&layout)?;
        for path in doomed {
            if let Some(removed) = remove_inside(&root, &path)? {
                report.bytes_freed += removed.bytes;
                report.removed.push(removed);
            }
        }
        if state == ManagedState::default() {
            // Nothing to remember: remove the bookkeeping too, and the root when nothing else is
            // left in it (it is never force-removed: unknown files are not ours to delete).
            let _ = std::fs::remove_file(layout.state_file());
            drop(lock);
            for bookkeeping in [layout.lock_file(), layout.marker_file()] {
                if let Some(removed) = remove_inside(&root, &bookkeeping)? {
                    report.removed.push(removed);
                }
            }
            report.root_removed = std::fs::remove_dir(&root).is_ok();
            if !report.root_removed {
                // Not empty: keep the marker so the leftovers stay recognisably ours.
                std::fs::write(layout.marker_file(), b"")
                    .map_err(|err| io_error("cannot restore the marker", err))?;
            }
        }
        Ok(report)
    }

    /// Wipes (see [`Self::teardown`]), provisions again and verifies: the bootstrap-resilience
    /// test, with a timing per phase.
    pub async fn rebuild(
        &self,
        options: &RebuildOptions,
        progress: ProgressFn<'_>,
    ) -> Result<RebuildReport, ProvisionError> {
        let started = Instant::now();
        progress(ProvisionEvent::Step(
            "wiping the managed Ollama state".to_owned(),
        ));
        let teardown_started = Instant::now();
        let teardown = self.teardown(&options.teardown).await?;
        let teardown_ms = elapsed_ms(teardown_started);

        let provision_started = Instant::now();
        let provision_options = ProvisionOptions {
            verify: false,
            ..options.provision.clone()
        };
        let provision = self.ensure_ready(&provision_options, progress).await?;
        let provision_ms = elapsed_ms(provision_started);

        let verify_started = Instant::now();
        let verification = self
            .verify_at(
                &provision.base_url,
                &provision.model,
                options.provision.verify_timeout,
            )
            .await?;
        Ok(RebuildReport {
            teardown,
            teardown_ms,
            provision,
            provision_ms,
            verification,
            verify_ms: elapsed_ms(verify_started),
            total_ms: elapsed_ms(started),
        })
    }

    /// The URL requests should go to now: the live managed server, else the configured URL.
    pub fn current_endpoint(&self) -> String {
        let (state, _) = ManagedState::load(&self.layout);
        self.live_server(&state)
            .map_or_else(|| self.config.base_url.clone(), |server| server.base_url)
    }

    /// Smoke test: a tiny deterministic generation with `model` on the current endpoint.
    pub async fn verify(&self, model: &str) -> Result<VerifyReport, ProvisionError> {
        self.verify_at(
            &self.current_endpoint(),
            model,
            ProvisionOptions::default().verify_timeout,
        )
        .await
    }

    async fn verify_at(
        &self,
        base_url: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<VerifyReport, ProvisionError> {
        let api = self.api(base_url, Duration::from_secs(2))?;
        let request = GenerateRequest {
            model: model.to_owned(),
            prompt: "Reply with the single word: ready".to_owned(),
            options: Some(serde_json::json!({ "num_predict": 8, "temperature": 0 })),
            stream: false,
            ..GenerateRequest::default()
        };
        let started = Instant::now();
        let response = api.generate(&request, Some(timeout)).await.map_err(|err| {
            ProvisionError::VerifyFailed {
                model: model.to_owned(),
                reason: err.to_string(),
            }
        })?;
        let completion_tokens = response.eval_count.unwrap_or(0);
        if completion_tokens == 0 && response.response.trim().is_empty() {
            return Err(ProvisionError::VerifyFailed {
                model: model.to_owned(),
                reason: "the model produced no output".to_owned(),
            });
        }
        Ok(VerifyReport {
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            latency_ms: elapsed_ms(started),
            prompt_tokens: response.prompt_eval_count.unwrap_or(0),
            completion_tokens,
            response: response.response.trim().chars().take(200).collect(),
        })
    }
}

/// `preferred` when it can be bound on loopback, else any free port.
fn pick_port(preferred: u16) -> Result<u16, ProvisionError> {
    for candidate in [preferred, 0] {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", candidate))
            && let Ok(addr) = listener.local_addr()
        {
            return Ok(addr.port());
        }
    }
    Err(ProvisionError::StartFailed(
        "no free loopback port available".to_owned(),
    ))
}
