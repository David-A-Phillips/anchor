//! # Anchor
//!
//! A local-first context engine and synchronisation daemon that grounds AI models in multi-repository
//! codebases, enterprise domains, wikis (including comment threads and dismissed discussions), API
//! approval portals and trackers.
//!
//! See `docs/ARCHITECTURE.md` for the design. Module map:
//!
//! * [`base`] — shared vocabulary, data model, storage handle and cross-module contracts;
//! * [`config`] — configuration, the interactive setup wizard and Ollama provisioning;
//! * [`engine`] — parsing, ingestion, the graph store and in-memory graph, embeddings, tiered model
//!   routing, the file watcher and the query engine;
//! * [`sync`] — Merkle trees, `.contextpack` manifests and the client/server protocol;
//! * [`backlog`] — the gap-analysis backlog and its background worker;
//! * [`metrics`] — the token ledger and resource monitoring;
//! * [`web`] — the embedded dashboard;
//! * [`ollama`] — the Ollama HTTP API client;
//! * [`cli`] — the command-line interface.

pub mod backlog;
pub mod base;
pub mod cli;
pub mod config;
pub mod engine;
pub mod metrics;
pub mod ollama;
pub mod schema;
pub mod sync;
pub mod web;

pub use crate::base::{Error, Result};
