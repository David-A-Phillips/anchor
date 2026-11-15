//! Backlog configuration (`[backlog]`).

use serde::{Deserialize, Serialize};

/// Backlog settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BacklogConfig {
    /// Record gaps and run the background worker.
    pub enabled: bool,
    /// Maximum number of open items kept; the lowest-priority items are evicted beyond it.
    pub max_open_items: usize,
    /// Seconds between background expansion passes when no new item arrives.
    pub worker_interval_secs: u64,
}

impl Default for BacklogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_open_items: 5000,
            worker_interval_secs: 300,
        }
    }
}
