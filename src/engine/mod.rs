//! The context engine.
//!
//! * [`parser`] — incremental Tree-sitter parsing and symbol extraction;
//! * [`ingest`] — format parsers that turn wikis, tracker exports, API portals, ownership files
//!   and org models into [`Unit`](crate::base::Unit)s;
//! * [`store`] — the SQLite system of record: units, the materialised graph, the linker, full-text
//!   search;
//! * [`graph`] — the compact in-memory petgraph and trust-weighted pathfinding;
//! * [`embed`] — local embeddings and the vector index;
//! * [`router`] — System 1 / System 2 model providers and tiered routing;
//! * [`pipeline`] — source scanning and unit application;
//! * [`watcher`] — file watching (notify) feeding the pipeline;
//! * [`query`] — the query engine tying everything together.

pub mod embed;
pub mod graph;
pub mod ingest;
pub mod parser;
pub mod pipeline;
pub mod query;
pub mod router;
pub mod store;
pub mod watcher;
