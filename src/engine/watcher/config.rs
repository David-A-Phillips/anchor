//! File watcher configuration (`[watcher]`).

use serde::{Deserialize, Serialize};

/// File watcher settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatcherConfig {
    /// Watch repositories for changes while the daemon runs.
    pub enabled: bool,
    /// Events for the same path arriving within this many milliseconds are coalesced. Part of the
    /// 50 ms change-to-graph latency budget, so keep it small.
    pub debounce_ms: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 8,
        }
    }
}
