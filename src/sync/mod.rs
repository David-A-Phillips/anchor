//! Synchronisation: Merkle trees over unit versions, signed `.contextpack` manifests, the merge
//! rule, and the client/server protocol over WebSockets and gRPC.

mod config;
pub mod identity;
pub mod merkle;
pub mod pack;
pub mod store;

use crate::base::Migration;

pub use config::{SyncConfig, SyncTransport};

/// Generated gRPC bindings for the sync protocol (`proto/anchor/sync/v1/sync.proto`).
pub mod pb {
    #![allow(missing_docs, clippy::all, clippy::unwrap_used)]
    tonic::include_proto!("anchor.sync.v1");
}

/// Schema history of the sync tables.
pub const MIGRATIONS: &[Migration] = &[];
