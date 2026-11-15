//! Dashboard configuration (`[web]`).

use serde::{Deserialize, Serialize};

/// Dashboard settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Serve the dashboard while the daemon runs.
    pub enabled: bool,
    /// Address to bind. Loopback by default: the dashboard is unauthenticated.
    pub bind: String,
    /// TCP port.
    pub port: u16,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".to_owned(),
            port: 7890,
        }
    }
}
