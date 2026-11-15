//! Metrics: the token ledger (usage, estimated cost and latency per tier) and resource monitoring.
//!
//! * [`TokenLedger`] — the SQLite-backed [`crate::base::UsageSink`] and its reporting queries;
//! * [`PriceTable`] — built-in and configured model prices, exact nano-dollar arithmetic;
//! * [`ResourceMonitor`] — cheap process CPU / memory sampling for the dashboard and for
//!   performance tests.

mod config;
mod ledger;
mod pricing;
mod resource;

use crate::base::Migration;

pub use config::{MetricsConfig, ModelPrice};
pub use ledger::{
    Bucket, DEFAULT_QUEUE_CAPACITY, LedgerEntry, LedgerSummary, ModelUsage, TimeBucket, TimeRange,
    TokenLedger, UsageStats, percentile,
};
pub use pricing::{
    ANTHROPIC_PROVIDER, CostEstimate, DEFAULT_BASELINE_MODEL, LongPromptTier, NANOS_PER_USD, Price,
    PriceTable, format_tokens, format_usd, format_usd_fixed, nanos_to_usd,
    usd_per_mtok_to_nanos_per_token,
};
pub use resource::{
    MIN_SAMPLER_INTERVAL, ProcessSample, ResourceMonitor, ResourceSnapshot, cpu_percent,
    thread_cpu_time,
};

/// Schema history of the metrics tables.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "create token ledger",
    sql: "CREATE TABLE token_ledger (
              id                     INTEGER PRIMARY KEY,
              ts_ms                  INTEGER NOT NULL,
              query_id               TEXT,
              tier                   INTEGER NOT NULL,
              locality               INTEGER NOT NULL,
              provider               TEXT    NOT NULL,
              model                  TEXT    NOT NULL,
              purpose                INTEGER NOT NULL,
              prompt_tokens          INTEGER NOT NULL,
              completion_tokens      INTEGER NOT NULL,
              cost_nanos             INTEGER NOT NULL,
              cloud_equivalent_nanos INTEGER NOT NULL,
              latency_ms             INTEGER NOT NULL,
              ok                     INTEGER NOT NULL,
              error                  TEXT,
              usage_estimated        INTEGER NOT NULL,
              priced                 INTEGER NOT NULL
          );
          CREATE INDEX token_ledger_ts ON token_ledger (ts_ms);
          CREATE INDEX token_ledger_query ON token_ledger (query_id) WHERE query_id IS NOT NULL;",
}];
