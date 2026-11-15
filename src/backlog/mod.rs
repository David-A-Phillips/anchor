//! The gap-analysis backlog: low-confidence queries and missing graph neighbours, recorded for
//! targeted background context expansion.

mod config;

use crate::base::Migration;

pub use config::BacklogConfig;

/// Schema history of the backlog tables.
pub const MIGRATIONS: &[Migration] = &[];
