//! [`RemoteTiles`] over a [`SyncClient`]: pulls the units one source needs at a depth.

use std::sync::Arc;

use tokio::runtime::Handle;

use crate::base::{Error, Result, SourceId};
use crate::config::Depth;
use crate::engine::RemoteTiles;

use super::client::{ClientOptions, SyncClient};
use super::store::UnitStore;

/// Pulls deeper tiles of single sources from the sync server.
pub struct SyncTiles {
    store: Arc<dyn UnitStore>,
    options: ClientOptions,
    runtime: Handle,
}

impl SyncTiles {
    /// Tiles pulled into `store` with the connection settings in `options` (its subscription,
    /// publication and depths are replaced per pull). `runtime` drives the async client.
    pub fn new(store: Arc<dyn UnitStore>, options: ClientOptions, runtime: Handle) -> Self {
        Self {
            store,
            options,
            runtime,
        }
    }
}

impl RemoteTiles for SyncTiles {
    /// Blocks; call from a blocking thread, not from async code.
    fn pull(&self, source: &SourceId, depth: Depth) -> Result<()> {
        let mut options = self.options.clone();
        options.publish = Vec::new();
        options.subscribe = vec![source.clone()];
        options.depths.insert(source.clone(), depth);
        let client = SyncClient::new(self.store.clone(), options);
        let report = self.runtime.block_on(client.sync_once())?;
        if report.rejected > 0 {
            return Err(Error::sync(format!(
                "the server's tiles for {source} were rejected ({} units)",
                report.rejected
            )));
        }
        Ok(())
    }
}

crate::base::opaque_debug!(SyncTiles);
