//! File watching (notify) feeding the ingestion pipeline.
//!
//! One recursive watcher covers every enabled source root. Events flow from notify's callback
//! thread through a channel to a dedicated ingest thread, which coalesces events per path within
//! [`WatcherConfig::debounce_ms`] and hands each changed path to
//! [`Engine::handle_path_change`]. Both threads block on their channels: an idle watcher uses no
//! CPU.

mod config;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use super::core::Engine;
use crate::base::{Error, Result};

pub use config::WatcherConfig;

enum Message {
    Changed(PathBuf, Instant),
    Stop,
}

/// A running file watcher. Dropping it (or calling [`FileWatcher::stop`]) stops watching.
pub struct FileWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    sender: mpsc::Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl FileWatcher {
    /// Starts watching every enabled source of the engine's configuration.
    pub fn start(engine: Arc<Engine>) -> Result<Self> {
        let debounce = Duration::from_millis(engine.config().watcher.debounce_ms);
        let (sender, receiver) = mpsc::channel::<Message>();
        let events = sender.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                let now = Instant::now();
                for path in event.paths {
                    if events.send(Message::Changed(path, now)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|err| Error::module("watcher", err))?;
        for source in engine
            .config()
            .sources
            .iter()
            .filter(|source| source.enabled)
        {
            let root = PathBuf::from(&source.location);
            if !root.exists() {
                continue;
            }
            if let Err(err) = watcher.watch(&root, RecursiveMode::Recursive) {
                tracing::warn!("cannot watch {}: {err}", root.display());
            }
        }

        let thread = std::thread::Builder::new()
            .name("anchor-watcher".into())
            .spawn(move || {
                // path -> first detection time of the pending change
                let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
                loop {
                    let message = if pending.is_empty() {
                        receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    } else {
                        receiver.recv_timeout(debounce)
                    };
                    match message {
                        Ok(Message::Changed(path, at)) => {
                            pending.entry(path).or_insert(at);
                            // Keep draining while events keep arriving within the window.
                            continue;
                        }
                        Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    for (path, detected) in pending.drain() {
                        if path.is_dir() {
                            continue;
                        }
                        if let Err(err) = engine.handle_path_change(&path, detected) {
                            tracing::warn!("ingesting {} failed: {err}", path.display());
                        }
                    }
                }
            })
            .map_err(Error::from)?;

        Ok(Self {
            watcher: Some(watcher),
            sender,
            thread: Some(thread),
        })
    }

    /// Stops watching and waits for the ingest thread to finish.
    pub fn stop(&mut self) {
        self.watcher.take();
        let _ = self.sender.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}
