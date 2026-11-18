//! Sync configuration (`[sync]`).

use serde::{Deserialize, Serialize};

use crate::base::SourceId;
use crate::config::Depth;

/// Wire protocol used between a client and the central server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTransport {
    /// WebSocket (`ws://` / `wss://`).
    #[default]
    #[serde(alias = "ws")]
    WebSocket,
    /// gRPC over HTTP/2.
    Grpc,
}

/// Client/server synchronisation settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// URL of the central sync server (client mode). `None` disables syncing.
    pub server_url: Option<String>,
    /// Transport used to reach the server.
    pub transport: SyncTransport,
    /// Name of the environment variable that holds the bearer token for the server.
    pub auth_token_env: Option<String>,
    /// Sources this device publishes to the server. Empty publishes nothing.
    pub publish: Vec<SourceId>,
    /// Sources this device pulls from the server. Empty pulls everything the server offers.
    pub subscribe: Vec<SourceId>,
    /// Depth at which subscribed sources without a local configuration are held (the rest of
    /// the depth comes from `anchor deepen`). `full` keeps the whole unit set materialised.
    pub default_depth: Depth,
    /// Seconds between sync rounds when no change notification arrives.
    pub interval_secs: u64,
    /// Address the sync server listens on (server mode). The default, `127.0.0.1:7891`, accepts
    /// connections from this machine only; set `0.0.0.0:7891` to serve other machines.
    pub listen: String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            transport: SyncTransport::WebSocket,
            auth_token_env: None,
            publish: Vec::new(),
            subscribe: Vec::new(),
            default_depth: Depth::Full,
            interval_secs: 30,
            listen: "127.0.0.1:7891".to_owned(),
        }
    }
}
