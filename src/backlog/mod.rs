//! The gap-analysis backlog: low-confidence queries and missing graph neighbours, recorded for
//! targeted background context expansion.
//!
//! * [`Backlog`] is the SQLite table with its operations: deduplicated recording, prioritised
//!   claiming with exponential backoff, state transitions, statistics, eviction and pruning.
//! * [`BacklogWorker`] is a Tokio task that claims items one at a time and hands them to a
//!   pluggable [`GapExpander`] (the real one lives in the query engine).
//!
//! # Gap keys
//!
//! Every report has a normalised *gap key*; reporting the same gap again increments `hits`,
//! refreshes `last_seen_ms` and keeps the lowest confidence instead of adding a row. Text is
//! lower-cased and whitespace-collapsed before it enters a key:
//!
//! | Kind | Key |
//! |------|-----|
//! | `low_confidence` | `low_confidence:<query>` |
//! | `missing_neighbor` | `missing_neighbor:<node key>\|<relation>` |
//! | `unresolved_entity` | `unresolved_entity:<mention>` |
//! | `conflict` | `conflict:<node key>\|<topic>` |
//!
//! A repeat of a *resolved* gap reopens it (the expansion evidently did not fix it); a repeat of a
//! *dismissed* gap only counts the hit; a repeat of an open or in-progress gap just updates it.
//!
//! # Priority
//!
//! ```text
//! base      = severity(kind) * (1 + ln(hits)) * (2 - confidence)        stored, updated on a hit
//! priority  = base / (1 + age_days / 7)                                 age = now - last_seen
//! ```
//!
//! Severity: conflict 3.0, missing neighbour 2.0, unresolved entity 1.5, low confidence 1.0.
//! Confidence is in `[0, 1]` (0.5 when the report has none), so an uncertain answer weighs up to
//! twice a confident one. Frequently hit gaps grow logarithmically; untouched gaps lose half their
//! weight after a week. Claiming, listing by priority and eviction all use the effective
//! priority at the time of the call; ties are broken by id.
//!
//! # Lifecycle
//!
//! ```text
//! open --claim_next--> in_progress --resolve--> resolved --reopen--> open
//!   ^                      |  \--dismiss--> dismissed --reopen--> open
//!   +-- defer / fail / release / recover_interrupted
//! ```
//!
//! `claim_next` counts an attempt and skips items whose last attempt is more recent than the
//! backoff `min(base * 2^(attempts - 1), cap)` (default base one minute, cap six hours).
//! [`Backlog::retry`] clears the backoff for a manual retry.

mod config;
mod store;
mod worker;

use crate::base::Migration;

pub use config::BacklogConfig;
pub use store::{
    Backlog, BacklogItem, BacklogStats, GapKind, GapReport, GapStatus, ListFilter, SortKey,
};
pub use worker::{BacklogWorker, ExpansionOutcome, GapExpander, WorkerHandle};

/// Schema history of the backlog tables.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "create backlog",
    sql: "CREATE TABLE backlog_items (
              id              INTEGER PRIMARY KEY,
              kind            INTEGER NOT NULL,
              gap_key         TEXT    NOT NULL UNIQUE,
              query           TEXT,
              intent          TEXT,
              confidence      REAL,
              details         TEXT    NOT NULL,
              hits            INTEGER NOT NULL,
              base_priority   REAL    NOT NULL,
              status          INTEGER NOT NULL,
              first_seen_ms   INTEGER NOT NULL,
              last_seen_ms    INTEGER NOT NULL,
              updated_ms      INTEGER NOT NULL,
              claimed_ms      INTEGER,
              attempts        INTEGER NOT NULL DEFAULT 0,
              last_attempt_ms INTEGER,
              last_error      TEXT,
              resolution_note TEXT,
              closed_ms       INTEGER
          );
          CREATE INDEX backlog_items_status ON backlog_items (status, base_priority DESC);",
}];
