//! Metrics: the token ledger (usage, estimated cost and latency per tier) and resource monitoring.

mod config;

use crate::base::Migration;

pub use config::{MetricsConfig, ModelPrice};

/// Schema history of the metrics tables.
pub const MIGRATIONS: &[Migration] = &[];
