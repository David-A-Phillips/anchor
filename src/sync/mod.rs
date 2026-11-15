//! Synchronisation: Merkle trees over unit versions, signed `.contextpack` manifests, the merge
//! rule, and the client/server protocol over WebSockets and gRPC.

mod client;
mod config;
pub mod graph_store;
pub mod identity;
pub mod merkle;
pub mod pack;
pub mod protocol;
mod server;
pub mod store;

#[cfg(test)]
mod tests;

use crate::base::Migration;

pub use client::{ClientOptions, SyncClient, SyncReport};
pub use config::{SyncConfig, SyncTransport};
pub use graph_store::{GraphUnitStore, OnApplied};
pub use server::{Change, ServerOptions, SyncServer, token_from_env, tokens_match};

/// Generated gRPC bindings for the sync protocol (`proto/anchor/sync/v1/sync.proto`).
pub mod pb {
    #![allow(missing_docs, clippy::all, clippy::unwrap_used)]
    tonic::include_proto!("anchor.sync.v1");
}

/// Schema history of the sync tables.
pub const MIGRATIONS: &[Migration] = &[];
