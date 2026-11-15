//! Ollama backend configuration (`[ollama]`).

use serde::{Deserialize, Serialize};

use crate::ollama::DEFAULT_BASE_URL;

/// Where Ollama lives and whether Anchor manages it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaConfig {
    /// Base URL of the Ollama server (localhost, or a LAN GPU box).
    pub base_url: String,
    /// Install, configure, boot and pull the default model automatically when no server is
    /// reachable at `base_url`.
    pub auto_provision: bool,
    /// `true` when the server at `base_url` was installed by Anchor and is managed by it. Only a
    /// managed installation is ever stopped, wiped or rebuilt.
    pub managed: bool,
    /// Model pulled during provisioning.
    pub default_model: String,
    /// Seconds to wait for a freshly started server to become healthy.
    pub startup_timeout_secs: u64,
    /// How long models stay loaded after a request (Ollama `keep_alive`).
    pub keep_alive: String,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            auto_provision: true,
            managed: false,
            default_model: "llama3.2:3b".to_owned(),
            startup_timeout_secs: 60,
            keep_alive: "10m".to_owned(),
        }
    }
}
