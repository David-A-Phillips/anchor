//! Test doubles for provisioning, public so that integration tests and the setup wizard's tests can
//! reuse them: an in-process fake Ollama server, a fake process launcher, a fake binary locator, a
//! local download server and builders for (deliberately broken) release archives.
//!
//! Nothing here touches the network beyond loopback or the real process table.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{
    BinaryLocator, LaunchSpec, OllamaManager, ProcessControl, ProvisionOptions, StopSignal,
    artifact_stem,
};
use crate::config::OllamaConfig;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Observable state of a [`FakeOllama`].
#[derive(Debug, Default)]
pub struct FakeOllamaState {
    /// Version string reported by `/api/version`.
    pub version: Mutex<String>,
    /// Models present.
    pub models: Mutex<Vec<String>>,
    /// Pulls requested, in order.
    pub pulls: Mutex<Vec<String>>,
    /// Deletions requested, in order.
    pub deletes: Mutex<Vec<String>>,
    /// Number of `/api/generate` calls served.
    pub generates: AtomicUsize,
    /// When set, every pull ends with this error message.
    pub pull_error: Mutex<Option<String>>,
    /// Directory in which pulled models leave a blob (persistence and size accounting).
    pub models_dir: Mutex<Option<PathBuf>>,
}

fn blob_name(model: &str) -> String {
    format!("{}.blob", model.replace('/', "%"))
}

fn same_model(a: &str, b: &str) -> bool {
    crate::ollama::normalize_model_name(a) == crate::ollama::normalize_model_name(b)
}

/// An in-process Ollama: `/api/version`, `/api/tags`, `/api/pull` (streaming progress),
/// `/api/delete`, `/api/generate`, `/api/ps`.
pub struct FakeOllama {
    port: u16,
    state: Arc<FakeOllamaState>,
    shutdown: CancellationToken,
}

impl FakeOllama {
    /// Starts a server on `127.0.0.1:port` (`0` picks a free port) with no models.
    pub async fn start(port: u16) -> io::Result<Self> {
        Self::start_with(port, &[], None).await
    }

    /// Starts a server holding `models`; with `models_dir`, models found there as blobs are
    /// present too and pulls leave blobs there.
    pub async fn start_with(
        port: u16,
        models: &[&str],
        models_dir: Option<&Path>,
    ) -> io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(FakeOllamaState::default());
        *lock(&state.version) = "0.0.0-fake".to_owned();
        let mut present: Vec<String> = models.iter().map(|m| (*m).to_owned()).collect();
        if let Some(dir) = models_dir {
            *lock(&state.models_dir) = Some(dir.to_path_buf());
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(model) = name.strip_suffix(".blob") {
                        present.push(model.replace('%', "/"));
                    }
                }
            }
        }
        *lock(&state.models) = present;

        let app = Router::new()
            .route("/api/version", get(version))
            .route("/api/tags", get(tags))
            .route("/api/ps", get(|| async { Json(json!({ "models": [] })) }))
            .route("/api/pull", post(pull))
            .route("/api/delete", delete(delete_model))
            .route("/api/generate", post(generate))
            .with_state(state.clone());
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await;
        });
        Ok(Self {
            port,
            state,
            shutdown,
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Its base URL.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Observable state; also lets tests change behaviour (`pull_error`, models).
    pub fn state(&self) -> &Arc<FakeOllamaState> {
        &self.state
    }

    /// Stops serving.
    pub fn stop(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for FakeOllama {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

use axum::Json;

async fn version(State(state): State<Arc<FakeOllamaState>>) -> Json<Value> {
    Json(json!({ "version": lock(&state.version).clone() }))
}

async fn tags(State(state): State<Arc<FakeOllamaState>>) -> Json<Value> {
    let models: Vec<Value> = lock(&state.models)
        .iter()
        .map(|name| {
            let name = if name.contains(':') {
                name.clone()
            } else {
                format!("{name}:latest")
            };
            json!({ "name": name, "size": 1000, "digest": "sha256:fake", "modified_at": "2026-01-01T00:00:00Z" })
        })
        .collect();
    Json(json!({ "models": models }))
}

fn ndjson(lines: Vec<Value>, delay: Duration) -> Response {
    let stream = futures_util::stream::iter(lines).then(move |line| async move {
        tokio::time::sleep(delay).await;
        Ok::<_, io::Error>(format!("{line}\n"))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

use futures_util::StreamExt as _;

async fn pull(State(state): State<Arc<FakeOllamaState>>, body: axum::Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    lock(&state.pulls).push(model.clone());
    let pull_error = lock(&state.pull_error).clone();
    if let Some(message) = pull_error {
        return ndjson(
            vec![
                json!({ "status": "pulling manifest" }),
                json!({ "error": message }),
            ],
            Duration::from_millis(1),
        );
    }
    {
        let mut models = lock(&state.models);
        if !models.iter().any(|m| same_model(m, &model)) {
            models.push(model.clone());
        }
    }
    let models_dir = lock(&state.models_dir).clone();
    if let Some(dir) = models_dir {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(blob_name(&model)), vec![0u8; 1000]);
    }
    let digest = "sha256:fakelayer";
    ndjson(
        vec![
            json!({ "status": "pulling manifest" }),
            json!({ "status": format!("pulling {digest}"), "digest": digest, "total": 1000, "completed": 0 }),
            json!({ "status": format!("pulling {digest}"), "digest": digest, "total": 1000, "completed": 500 }),
            json!({ "status": format!("pulling {digest}"), "digest": digest, "total": 1000, "completed": 1000 }),
            json!({ "status": "verifying sha256 digest" }),
            json!({ "status": "success" }),
        ],
        Duration::from_millis(2),
    )
}

async fn delete_model(
    State(state): State<Arc<FakeOllamaState>>,
    body: axum::Json<Value>,
) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    lock(&state.deletes).push(model.clone());
    let mut models = lock(&state.models);
    let before = models.len();
    models.retain(|m| !same_model(m, &model));
    if models.len() == before {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": format!("model '{model}' not found") })),
        )
            .into_response();
    }
    let models_dir = lock(&state.models_dir).clone();
    if let Some(dir) = models_dir {
        let _ = std::fs::remove_file(dir.join(blob_name(&model)));
    }
    StatusCode::OK.into_response()
}

async fn generate(State(state): State<Arc<FakeOllamaState>>, body: axum::Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    if !lock(&state.models).iter().any(|m| same_model(m, &model)) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": format!("model '{model}' not found") })),
        )
            .into_response();
    }
    state.generates.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({
        "model": model, "response": "ready", "done": true, "done_reason": "stop",
        "prompt_eval_count": 7, "eval_count": 2, "total_duration": 1_000_000
    }))
    .into_response()
}

#[derive(Default)]
struct LauncherInner {
    next_pid: u32,
    servers: HashMap<u32, Option<FakeOllama>>,
    specs: Vec<LaunchSpec>,
}

/// A [`ProcessControl`] that "starts" an in-process [`FakeOllama`] on the requested port instead
/// of a real process.
#[derive(Default)]
pub struct FakeLauncher {
    inner: Mutex<LauncherInner>,
    /// Spawn fails with this message.
    pub fail_spawn: Mutex<Option<String>>,
    /// The process "runs" but never serves (health timeout).
    pub never_healthy: Mutex<bool>,
    /// Pulls on spawned servers fail with this message.
    pub pull_error: Mutex<Option<String>>,
    /// Models every spawned server already has (besides blobs in its models directory).
    pub initial_models: Mutex<Vec<String>>,
}

impl FakeLauncher {
    /// A launcher with default behaviour.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(LauncherInner {
                next_pid: 4_100_000,
                ..LauncherInner::default()
            }),
            ..Self::default()
        })
    }

    /// Every launch request seen.
    pub fn specs(&self) -> Vec<LaunchSpec> {
        lock(&self.inner).specs.clone()
    }

    /// Number of fake processes currently running.
    pub fn running(&self) -> usize {
        lock(&self.inner).servers.len()
    }

    /// Simulates a crash of every running fake server (pids vanish).
    pub fn crash_all(&self) {
        let mut inner = lock(&self.inner);
        for server in inner.servers.values().flatten() {
            server.stop();
        }
        inner.servers.clear();
    }

    /// The servers still alive, for inspecting their state.
    pub fn server_states(&self) -> Vec<Arc<FakeOllamaState>> {
        lock(&self.inner)
            .servers
            .values()
            .flatten()
            .map(|server| server.state().clone())
            .collect()
    }
}

#[async_trait]
impl ProcessControl for FakeLauncher {
    async fn spawn(&self, spec: &LaunchSpec) -> io::Result<u32> {
        let fail_spawn = lock(&self.fail_spawn).clone();
        if let Some(message) = fail_spawn {
            return Err(io::Error::other(message));
        }
        let server = if *lock(&self.never_healthy) {
            None
        } else {
            let initial = lock(&self.initial_models).clone();
            let initial: Vec<&str> = initial.iter().map(String::as_str).collect();
            let server =
                FakeOllama::start_with(spec.port, &initial, Some(&spec.models_dir)).await?;
            *lock(&server.state().pull_error) = lock(&self.pull_error).clone();
            Some(server)
        };
        let mut inner = lock(&self.inner);
        inner.next_pid += 1;
        let pid = inner.next_pid;
        inner.specs.push(spec.clone());
        inner.servers.insert(pid, server);
        // Like the real thing: the log file exists once the process is started.
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_file);
        Ok(pid)
    }

    fn is_running(&self, pid: u32, _binary: &Path) -> bool {
        lock(&self.inner).servers.contains_key(&pid)
    }

    fn signal(&self, pid: u32, _signal: StopSignal) -> io::Result<()> {
        let removed = lock(&self.inner).servers.remove(&pid);
        if let Some(Some(server)) = removed {
            server.stop();
        }
        Ok(())
    }
}

/// A [`BinaryLocator`] with a fixed answer.
#[derive(Clone, Debug, Default)]
pub struct FixedLocator(pub Option<PathBuf>);

impl BinaryLocator for FixedLocator {
    fn system_ollama(&self) -> Option<PathBuf> {
        self.0.clone()
    }
}

/// Serves fixed files over HTTP on loopback (any query string is ignored), counting requests.
pub struct FakeDownloadServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    shutdown: CancellationToken,
}

type Files = Arc<BTreeMap<String, Vec<u8>>>;

impl FakeDownloadServer {
    /// Starts a server for `files` (path without leading slash -> bytes).
    pub async fn start(files: BTreeMap<String, Vec<u8>>) -> io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new()
            .route(
                "/{*path}",
                get(
                    |State(files): State<Files>, UrlPath(path): UrlPath<String>| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        match files.get(&path) {
                            Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .with_state(Arc::new(files));
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await;
        });
        Ok(Self {
            port,
            hits,
            shutdown,
        })
    }

    /// Serves the standard fake release (`archive_name()` and `sha256sum.txt`).
    pub async fn start_release() -> io::Result<Self> {
        Self::start(release_files(&distribution_tar_zst())).await
    }

    /// Base URL to use as `ProvisionOptions::download_base_url`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Requests served so far.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for FakeDownloadServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Name of the archive the installer asks for on this platform.
pub fn archive_name() -> String {
    let stem = artifact_stem(std::env::consts::OS, std::env::consts::ARCH)
        .unwrap_or_else(|_| "ollama-linux-amd64".to_owned());
    format!("{stem}.tar.zst")
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Files of a release: the archive under [`archive_name`] plus a correct `sha256sum.txt`.
pub fn release_files(archive: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let name = archive_name();
    let listing = format!("{}  ./{name}\n", sha256_hex(archive));
    BTreeMap::from([
        (name, archive.to_vec()),
        ("sha256sum.txt".to_owned(), listing.into_bytes()),
    ])
}

fn tar_with(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, content, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(*mode);
        header.set_entry_type(tar::EntryType::Regular);
        // Written raw so that hostile names (`../x`, `/abs`) can be produced on purpose.
        let raw = header.as_old_mut();
        raw.name = [0; 100];
        raw.name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        let _ = builder.append(&header, *content);
    }
    builder.into_inner().unwrap_or_default()
}

/// A minimal valid distribution: `bin/ollama` (executable stub) and one library.
pub fn distribution_tar() -> Vec<u8> {
    tar_with(&[
        ("bin/ollama", b"#!/bin/sh\nexit 0\n", 0o755),
        ("lib/ollama/libfake.so", &[7u8; 2048], 0o644),
    ])
}

/// A tar containing the given `(name, content)` files, all mode 0644 (for hostile-archive tests).
pub fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let entries: Vec<(&str, &[u8], u32)> = entries.iter().map(|(n, c)| (*n, *c, 0o644)).collect();
    tar_with(&entries)
}

/// zstd-compresses `bytes`.
pub fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 3).unwrap_or_default()
}

/// gzip-compresses `bytes`.
pub fn gzip_compress(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

/// [`distribution_tar`], zstd-compressed.
pub fn distribution_tar_zst() -> Vec<u8> {
    zstd_compress(&distribution_tar())
}

/// A complete hermetic environment: temp directory, fake launcher, local download server and a
/// manager wired to them. The configured endpoint is a free port with nothing listening.
pub struct TestRig {
    /// Temporary directory holding everything (kept alive by the rig).
    pub dir: tempfile::TempDir,
    /// The fake process launcher.
    pub launcher: Arc<FakeLauncher>,
    /// The local download server.
    pub downloads: FakeDownloadServer,
    /// The manager under test.
    pub manager: OllamaManager,
    /// Options pointing at the local download server, with short timeouts.
    pub options: ProvisionOptions,
    /// The configuration the manager was built with.
    pub config: OllamaConfig,
}

/// A loopback port that is free right now and will not be handed to anyone else in this process.
///
/// Binding port `0` and closing the socket again is racy: the kernel may hand the same ephemeral
/// port to the next `bind(0)` of any concurrently running test (a fake download server, another
/// rig) before the caller uses it, so tests saw each other's servers ("address already in use", or
/// a server that was meant to be absent answering). Ports are therefore drawn from a private range
/// below the OS ephemeral range through a process-wide counter: no `bind(0)` can land on them and
/// no two callers receive the same port. Each candidate is still probed, to skip ports held by
/// unrelated processes.
pub fn free_port() -> u16 {
    use std::sync::atomic::AtomicU32;
    const FIRST: u32 = 16_000;
    const SPAN: u32 = 12_000;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    // Different test binaries and re-runs start at different offsets within the range.
    let start = std::process::id().wrapping_mul(2_654_435_761) % SPAN;
    for _ in 0..SPAN {
        let step = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = FIRST + (start + step) % SPAN;
        if let Ok(port) = u16::try_from(port)
            && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
        {
            return port;
        }
    }
    0
}

impl TestRig {
    /// Rig without a system `ollama`.
    pub async fn new() -> io::Result<Self> {
        Self::build(None, FakeDownloadServer::start_release().await?)
    }

    /// Rig where `ollama` is found on `PATH` at `binary`.
    pub async fn with_system_binary(binary: PathBuf) -> io::Result<Self> {
        Self::build(Some(binary), FakeDownloadServer::start_release().await?)
    }

    /// Rig with a custom download server (hostile archives, wrong checksums).
    pub fn with_downloads(downloads: FakeDownloadServer) -> io::Result<Self> {
        Self::build(None, downloads)
    }

    fn build(system: Option<PathBuf>, downloads: FakeDownloadServer) -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let port = free_port();
        let config = OllamaConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            default_model: "tiny:latest".to_owned(),
            startup_timeout_secs: 5,
            ..OllamaConfig::default()
        };
        let launcher = FakeLauncher::new();
        let manager = OllamaManager::with_parts(
            config.clone(),
            dir.path().join("data").join("ollama"),
            launcher.clone(),
            Arc::new(FixedLocator(system)),
        )
        .with_home(Some(dir.path().join("home")));
        let options = ProvisionOptions {
            download_base_url: downloads.url(),
            preferred_port: Some(port),
            probe_timeout: Duration::from_millis(400),
            startup_timeout: Some(Duration::from_secs(5)),
            verify_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
            ..ProvisionOptions::default()
        };
        Ok(Self {
            dir,
            launcher,
            downloads,
            manager,
            options,
            config,
        })
    }

    /// The managed root.
    pub fn root(&self) -> &Path {
        self.manager.layout().root()
    }
}

crate::base::opaque_debug!(FakeOllama, FakeLauncher, FakeDownloadServer, TestRig);
