//! Process launching, liveness and signalling behind small traits, so that hermetic tests can
//! substitute an in-process fake server for the real `ollama serve`.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

/// Everything needed to start `ollama serve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    /// The `ollama` executable.
    pub binary: PathBuf,
    /// Loopback address the server binds (`OLLAMA_HOST` is `host:port`).
    pub host: String,
    /// Port the server binds.
    pub port: u16,
    /// Model store (`OLLAMA_MODELS`).
    pub models_dir: PathBuf,
    /// `HOME` of the server process, so that Ollama's key pair lives inside the managed root
    /// instead of `~/.ollama`.
    pub home_dir: PathBuf,
    /// File receiving the server's stdout and stderr (appended).
    pub log_file: PathBuf,
}

impl LaunchSpec {
    /// Value of `OLLAMA_HOST`.
    pub fn ollama_host(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// How to ask a process to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopSignal {
    /// Polite request (`SIGTERM`): Ollama unloads its models and exits.
    Terminate,
    /// Forced stop (`SIGKILL`) of the server and its process group.
    Kill,
}

/// Starts, inspects and signals the managed server process.
#[async_trait]
pub trait ProcessControl: Send + Sync {
    /// Starts the server detached from the calling process (it must outlive it) and returns its
    /// process id. Must not wait for the server to become healthy.
    async fn spawn(&self, spec: &LaunchSpec) -> io::Result<u32>;

    /// `true` only when process `pid` is alive **and** verifiably runs `binary`. This is what makes
    /// a recorded pid safe to signal after the operating system may have reused it.
    fn is_running(&self, pid: u32, binary: &Path) -> bool;

    /// Sends `signal` to process `pid`. Callers check [`ProcessControl::is_running`] first.
    fn signal(&self, pid: u32, signal: StopSignal) -> io::Result<()>;
}

/// Finds an Ollama executable installed on the system.
pub trait BinaryLocator: Send + Sync {
    /// Path of a system `ollama` binary, when one is on `PATH`.
    fn system_ollama(&self) -> Option<PathBuf>;
}

/// [`BinaryLocator`] searching the real `PATH`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PathLocator;

impl BinaryLocator for PathLocator {
    fn system_ollama(&self) -> Option<PathBuf> {
        which::which("ollama").ok()
    }
}

/// [`ProcessControl`] acting on real operating-system processes. Supported on Linux; elsewhere
/// every operation reports that a managed server is unavailable.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcesses;

#[cfg(target_os = "linux")]
#[async_trait]
impl ProcessControl for SystemProcesses {
    async fn spawn(&self, spec: &LaunchSpec) -> io::Result<u32> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_file)?;
        let child = Command::new(&spec.binary)
            .arg("serve")
            .env("OLLAMA_HOST", spec.ollama_host())
            .env("OLLAMA_MODELS", &spec.models_dir)
            .env("HOME", &spec.home_dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group: a Ctrl-C in the terminal that ran `anchor setup` must not
            // reach the server, and the group can be killed as a whole (model runners included).
            .process_group(0)
            .spawn()?;
        let pid = child.id();
        // Reap the child if it exits while this process is still alive, so it never lingers as a
        // zombie; if this process exits first, init adopts and reaps it.
        std::thread::Builder::new()
            .name("ollama-reaper".to_owned())
            .spawn(move || {
                let mut child = child;
                let _ = child.wait();
            })?;
        Ok(pid)
    }

    fn is_running(&self, pid: u32, binary: &Path) -> bool {
        // A zombie has no `exe` link, so it counts as dead, which is what callers want.
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        let Ok(expected) = std::fs::canonicalize(binary) else {
            return false;
        };
        if exe != expected {
            return false;
        }
        // Same executable is not enough: `ollama run` is not the server.
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|cmdline| cmdline.split(|byte| *byte == 0).any(|arg| arg == b"serve"))
            .unwrap_or(false)
    }

    fn signal(&self, pid: u32, signal: StopSignal) -> io::Result<()> {
        use nix::sys::signal::{Signal, kill, killpg};
        use nix::unistd::Pid;

        let raw = i32::try_from(pid)
            .ok()
            .filter(|raw| *raw > 1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))?;
        let pid = Pid::from_raw(raw);
        let result = match signal {
            StopSignal::Terminate => kill(pid, Signal::SIGTERM),
            // The server leads its own group (see `spawn`), so this also takes down model runners
            // that would otherwise keep GPU memory. Fall back to the single process when the
            // server was not started as a group leader.
            StopSignal::Kill => killpg(pid, Signal::SIGKILL).or_else(|_| kill(pid, Signal::SIGKILL)),
        };
        match result {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[async_trait]
impl ProcessControl for SystemProcesses {
    async fn spawn(&self, _spec: &LaunchSpec) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Anchor can only run a managed Ollama server on Linux; start Ollama yourself and set ollama.base_url",
        ))
    }

    fn is_running(&self, _pid: u32, _binary: &Path) -> bool {
        false
    }

    fn signal(&self, _pid: u32, _signal: StopSignal) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process control is only supported on Linux",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_reused_pid_is_not_mistaken_for_the_server() {
        // This test process is alive but is not `ollama serve`.
        let me = std::process::id();
        let processes = SystemProcesses;
        assert!(!processes.is_running(me, Path::new("/definitely/not/ollama")));
        let own_exe = std::env::current_exe().expect("current exe");
        assert!(
            !processes.is_running(me, &own_exe),
            "same executable without the `serve` argument is not the server"
        );
        assert!(processes.signal(0, StopSignal::Kill).is_err());
        assert!(processes.signal(1, StopSignal::Kill).is_err());
    }
}
