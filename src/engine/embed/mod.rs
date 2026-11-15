//! Local embeddings and the vector index.

mod config;

use crate::base::Migration;

pub use config::{EmbeddingBackend, EmbeddingConfig};

/// Schema history of the embedding tables.
pub const MIGRATIONS: &[Migration] = &[];
