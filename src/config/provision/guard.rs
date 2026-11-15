//! The managed root: its layout, the state file, the inter-process lock and the safety checks that
//! every deletion goes through.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ProvisionError;

/// Name of the file marking a directory as created and owned by Anchor. Nothing is ever deleted
/// from a directory that does not contain it.
pub const MARKER_FILE: &str = ".anchor-managed";

const MARKER_CONTENT: &str = "This directory is managed by Anchor (local Ollama installation).\n\
`anchor` may delete its contents. Do not store anything else here.\n";

/// Paths inside the managed root (`AnchorPaths::ollama_dir()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedLayout {
    root: PathBuf,
}

impl ManagedLayout {
    /// Layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The managed root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The extracted Ollama distribution (`bin/ollama`, `lib/ollama/...`).
    pub fn dist_dir(&self) -> PathBuf {
        self.root.join("dist")
    }

    /// The `ollama` executable of the managed distribution.
    pub fn binary(&self) -> PathBuf {
        self.dist_dir().join("bin").join("ollama")
    }

    /// Model store of the managed server (`OLLAMA_MODELS`).
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// `HOME` of the managed server (holds Ollama's key pair).
    pub fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Install manifest and runtime state.
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Server logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Log of the managed server.
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("ollama.log")
    }

    /// Downloaded archives.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Lock serialising provisioning operations across processes.
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("lock")
    }

    /// The marker identifying the directory as Anchor-managed.
    pub fn marker_file(&self) -> PathBuf {
        self.root.join(MARKER_FILE)
    }

    /// `true` when the root exists and carries the marker.
    pub fn is_marked(&self) -> bool {
        std::fs::symlink_metadata(self.marker_file()).is_ok_and(|meta| meta.is_file())
    }
}

/// Where the server binary came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinarySource {
    /// An `ollama` found on `PATH`; Anchor runs it but never removes it.
    System,
    /// The distribution Anchor downloaded into the managed root.
    Managed,
}

/// What was installed into the managed root, and from where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallManifest {
    /// Version reported by the installed server (known after its first start).
    #[serde(default)]
    pub version: Option<String>,
    /// Version that was requested (`None`: the latest release).
    #[serde(default)]
    pub requested_version: Option<String>,
    /// URL the archive was downloaded from.
    pub source_url: String,
    /// File name of the archive in the download cache.
    pub archive: String,
    /// SHA-256 of the archive, lowercase hex.
    pub archive_sha256: String,
    /// `true` when the checksum was compared against a published or caller-supplied value.
    #[serde(default)]
    pub checksum_verified: bool,
    /// Size of the archive in bytes.
    pub archive_size: u64,
    /// When the distribution was installed (milliseconds since the Unix epoch).
    pub installed_at: i64,
}

/// The managed server process as last started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRecord {
    /// Process id.
    pub pid: u32,
    /// Loopback port it listens on.
    pub port: u16,
    /// Base URL of its API.
    pub base_url: String,
    /// Executable it runs; used to verify the pid before signalling it.
    pub binary: PathBuf,
    /// Where that executable came from.
    pub binary_source: BinarySource,
    /// Start time (milliseconds since the Unix epoch).
    pub started_at: i64,
    /// Version it reported once healthy.
    #[serde(default)]
    pub version: Option<String>,
}

/// A model Anchor pulled, and where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulledModel {
    /// Model name as requested.
    pub name: String,
    /// Server it was pulled on.
    pub base_url: String,
    /// `true` when that server was the managed one (the model lives in the managed root).
    pub managed: bool,
    /// When (milliseconds since the Unix epoch).
    pub pulled_at: i64,
}

/// Contents of `state.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedState {
    /// The installed distribution, if any.
    pub install: Option<InstallManifest>,
    /// The managed server as last started, if any. May be stale; always verified before use.
    pub server: Option<ServerRecord>,
    /// Models pulled by Anchor (and only those).
    pub models: Vec<PulledModel>,
}

pub(super) fn io_error(context: impl Into<String>, source: io::Error) -> ProvisionError {
    ProvisionError::Io {
        context: context.into(),
        source,
    }
}

impl ManagedState {
    /// Reads the state file. A missing file is the empty state; an unreadable or corrupt one is
    /// also treated as empty (and reported through the flag) because everything it records is
    /// re-verified against reality anyway.
    pub(super) fn load(layout: &ManagedLayout) -> (Self, bool) {
        match std::fs::read(layout.state_file()) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(state) => (state, false),
                Err(err) => {
                    tracing::warn!(error = %err, "ignoring corrupt Ollama state file");
                    (Self::default(), true)
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => (Self::default(), false),
            Err(err) => {
                tracing::warn!(error = %err, "ignoring unreadable Ollama state file");
                (Self::default(), true)
            }
        }
    }

    /// Writes the state atomically; an empty state removes the file.
    pub(super) fn save(&self, layout: &ManagedLayout) -> Result<(), ProvisionError> {
        let path = layout.state_file();
        if *self == Self::default() {
            return match std::fs::remove_file(&path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => {
                    Err(io_error("cannot remove the Ollama state file", err))
                }
                _ => Ok(()),
            };
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| io_error("cannot encode the Ollama state", io::Error::other(err)))?;
        let temporary = layout.root().join("state.json.tmp");
        std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|err| io_error("cannot write the Ollama state file", err))
    }
}

/// Creates the managed root and marks it. Refuses a pre-existing, non-empty directory without the
/// marker: Anchor never adopts a directory it did not create.
pub(super) fn prepare_root(layout: &ManagedLayout) -> Result<(), ProvisionError> {
    let root = layout.root();
    if layout.is_marked() {
        return Ok(());
    }
    match std::fs::read_dir(root) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(ProvisionError::UnsafePath {
                    path: root.to_path_buf(),
                    reason: "the directory exists, is not empty and was not created by Anchor"
                        .to_owned(),
                });
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root)
                .map_err(|err| io_error(format!("cannot create {}", root.display()), err))?;
        }
        Err(err) => return Err(io_error(format!("cannot read {}", root.display()), err)),
    }
    std::fs::write(layout.marker_file(), MARKER_CONTENT)
        .map_err(|err| io_error("cannot write the managed-directory marker", err))
}

/// Checks that `root` may be deleted from and returns its canonical path.
///
/// Refused: a root that cannot be canonicalised, the filesystem root or a top-level directory, a
/// home directory or any ancestor of one, Ollama's own `~/.ollama`, and any directory without the
/// marker file.
pub(super) fn check_root(root: &Path, home: Option<&Path>) -> Result<PathBuf, ProvisionError> {
    let refuse = |reason: &str| ProvisionError::UnsafePath {
        path: root.to_path_buf(),
        reason: reason.to_owned(),
    };
    let canonical = std::fs::canonicalize(root)
        .map_err(|err| refuse(&format!("cannot resolve the directory: {err}")))?;
    let Some(parent) = canonical.parent() else {
        return Err(refuse("it is the filesystem root"));
    };
    if parent.parent().is_none() {
        return Err(refuse("it is a top-level system directory"));
    }
    if parent == Path::new("/home") || parent == Path::new("/Users") || canonical == Path::new("/root")
    {
        return Err(refuse("it is a home directory"));
    }
    if let Some(home) = home {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(&canonical) {
            return Err(refuse("it is, or contains, the home directory"));
        }
    }
    if canonical.file_name().is_some_and(|name| name == ".ollama") {
        return Err(refuse("it is Ollama's own data directory"));
    }
    let marked = std::fs::symlink_metadata(canonical.join(MARKER_FILE)).is_ok_and(|m| m.is_file());
    if !marked {
        return Err(refuse(
            "it does not contain the Anchor marker file, so it was not created by Anchor",
        ));
    }
    Ok(canonical)
}

/// A path that was deleted and the bytes it held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RemovedPath {
    /// What was removed.
    pub path: PathBuf,
    /// Bytes freed (apparent size of the files).
    pub bytes: u64,
}

fn tree_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

/// Deletes `target`, which must resolve to a location strictly inside `canonical_root`. Symbolic
/// links are removed, never followed. Returns `None` when there was nothing to remove.
pub(super) fn remove_inside(
    canonical_root: &Path,
    target: &Path,
) -> Result<Option<RemovedPath>, ProvisionError> {
    let refuse = |reason: &str| ProvisionError::UnsafePath {
        path: target.to_path_buf(),
        reason: reason.to_owned(),
    };
    let meta = match std::fs::symlink_metadata(target) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(format!("cannot inspect {}", target.display()), err)),
    };
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(refuse("it has no parent directory"));
    };
    // Resolve the parent (which may involve symlinks) but not the entry itself: a symlink placed
    // inside the root must be unlinked, not chased to whatever it points at.
    let parent = std::fs::canonicalize(parent)
        .map_err(|err| refuse(&format!("cannot resolve its parent: {err}")))?;
    if !parent.starts_with(canonical_root) {
        return Err(refuse("it is outside the managed directory"));
    }
    let resolved = parent.join(name);
    if resolved == canonical_root {
        return Err(refuse("it is the managed directory itself"));
    }
    let (bytes, result) = if meta.is_dir() {
        (tree_size(&resolved), std::fs::remove_dir_all(&resolved))
    } else {
        let bytes = if meta.is_file() { meta.len() } else { 0 };
        (bytes, std::fs::remove_file(&resolved))
    };
    result.map_err(|err| io_error(format!("cannot remove {}", resolved.display()), err))?;
    Ok(Some(RemovedPath {
        path: resolved,
        bytes,
    }))
}

/// Exclusive inter-process lock on the managed root, released on drop.
#[derive(Debug)]
pub(super) struct RootLock {
    #[cfg(unix)]
    _lock: nix::fcntl::Flock<std::fs::File>,
    #[cfg(not(unix))]
    _file: std::fs::File,
}

impl RootLock {
    /// Waits up to `timeout` for the lock. The wait polls: `flock` has no timed variant, and a
    /// blocked thread could not be cancelled.
    pub(super) async fn acquire(path: &Path, timeout: Duration) -> Result<Self, ProvisionError> {
        let started = Instant::now();
        loop {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|err| io_error(format!("cannot open the lock {}", path.display()), err))?;
            #[cfg(unix)]
            {
                use nix::fcntl::{Flock, FlockArg};
                match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                    Ok(lock) => return Ok(Self { _lock: lock }),
                    Err((_, nix::errno::Errno::EWOULDBLOCK)) => {}
                    Err((_, errno)) => {
                        return Err(io_error(
                            format!("cannot lock {}", path.display()),
                            io::Error::from(errno),
                        ));
                    }
                }
            }
            #[cfg(not(unix))]
            {
                // No managed installation exists on these platforms, so there is nothing two
                // processes could corrupt; the file is only held open.
                return Ok(Self { _file: file });
            }
            #[cfg(unix)]
            {
                if started.elapsed() >= timeout {
                    return Err(ProvisionError::LockTimeout {
                        path: path.to_path_buf(),
                        waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked_root(dir: &Path) -> ManagedLayout {
        let layout = ManagedLayout::new(dir.join("data").join("ollama"));
        prepare_root(&layout).expect("prepare");
        layout
    }

    #[test]
    fn unmarked_or_dangerous_roots_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = marked_root(dir.path());
        assert!(check_root(layout.root(), None).is_ok());

        // The home directory, or an ancestor of it, even when marked.
        let err = check_root(layout.root(), Some(layout.root())).expect_err("home");
        assert!(err.to_string().contains("home"), "{err}");
        assert!(check_root(layout.root(), Some(&layout.root().join("user"))).is_err());

        let unmarked = dir.path().join("plain");
        std::fs::create_dir_all(&unmarked).expect("mkdir");
        let err = check_root(&unmarked, None).expect_err("unmarked");
        assert!(err.to_string().contains("marker"), "{err}");
        assert!(check_root(Path::new("/"), None).is_err());
        assert!(check_root(Path::new("/tmp"), None).is_err());
        assert!(check_root(&dir.path().join("missing"), None).is_err());

        let dot_ollama = dir.path().join(".ollama");
        std::fs::create_dir_all(&dot_ollama).expect("mkdir");
        std::fs::write(dot_ollama.join(MARKER_FILE), "x").expect("marker");
        assert!(check_root(&dot_ollama, None).is_err());

        // A non-empty directory Anchor did not create is never adopted.
        std::fs::write(unmarked.join("file"), "x").expect("write");
        assert!(prepare_root(&ManagedLayout::new(&unmarked)).is_err());
        assert!(!unmarked.join(MARKER_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn removal_never_leaves_the_root_or_follows_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = marked_root(dir.path());
        let root = check_root(layout.root(), None).expect("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("precious"), "keep me").expect("write");

        assert!(remove_inside(&root, &outside).is_err());
        assert!(remove_inside(&root, &root).is_err());
        assert!(remove_inside(&root, &root.join("models").join("..").join("..")).is_err());
        assert_eq!(remove_inside(&root, &root.join("absent")).expect("absent"), None);

        // A symlink inside the root pointing outside: the link goes, the target stays.
        std::os::unix::fs::symlink(&outside, root.join("models")).expect("symlink");
        let removed = remove_inside(&root, &root.join("models")).expect("unlink").expect("some");
        assert_eq!(removed.bytes, 0);
        assert!(outside.join("precious").exists());

        // A path that reaches outside through a symlinked parent is refused.
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
        assert!(remove_inside(&root, &root.join("link").join("precious")).is_err());
        assert!(outside.join("precious").exists());

        std::fs::create_dir_all(root.join("cache/sub")).expect("mkdir");
        std::fs::write(root.join("cache/sub/a"), [0u8; 10]).expect("write");
        let removed = remove_inside(&root, &root.join("cache")).expect("rm").expect("some");
        assert_eq!(removed.bytes, 10);
        assert!(!root.join("cache").exists());
    }

    #[tokio::test]
    async fn lock_is_exclusive_and_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let held = RootLock::acquire(&path, Duration::from_secs(1)).await.expect("first");
        if cfg!(unix) {
            let err = RootLock::acquire(&path, Duration::from_millis(120))
                .await
                .expect_err("second must wait");
            assert!(matches!(err, ProvisionError::LockTimeout { .. }));
        }
        drop(held);
        RootLock::acquire(&path, Duration::from_secs(1)).await.expect("after release");
    }

    #[test]
    fn state_round_trips_and_corruption_is_recovered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = marked_root(dir.path());
        let state = ManagedState {
            models: vec![PulledModel {
                name: "m".to_owned(),
                base_url: "http://127.0.0.1:1".to_owned(),
                managed: true,
                pulled_at: 1,
            }],
            ..ManagedState::default()
        };
        state.save(&layout).expect("save");
        assert_eq!(ManagedState::load(&layout), (state, false));
        std::fs::write(layout.state_file(), "{ not json").expect("corrupt");
        assert_eq!(ManagedState::load(&layout), (ManagedState::default(), true));
        ManagedState::default().save(&layout).expect("save empty");
        assert!(!layout.state_file().exists());
    }
}
