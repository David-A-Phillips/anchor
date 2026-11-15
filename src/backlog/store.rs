//! The backlog table and its operations. See the module documentation of [`crate::backlog`] for
//! gap keys, the priority formula and the item lifecycle.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::types::{ToSql, Type, Value};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, named_params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Notify;

use crate::base::{Db, Error, QueryIntent, Result, now_ms, string_enum};

use super::config::BacklogConfig;

const DAY_MS: f64 = 86_400_000.0;
/// Half-life style constant of the recency factor: weight halves after one week.
const RECENCY_SCALE_MS: f64 = 7.0 * DAY_MS;

/// Default base of the exponential retry backoff.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(60);
/// Default cap of the exponential retry backoff.
const DEFAULT_BACKOFF_CAP: Duration = Duration::from_secs(6 * 3600);
/// Default number of attempts after which a failing item is dismissed by [`Backlog::fail`].
const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// SQL expression of the effective (recency-decayed) priority; needs the `:now` parameter.
const EFFECTIVE: &str = "(base_priority / (1.0 + MAX(:now - last_seen_ms, 0) / 604800000.0))";

/// SQL expression of the moment an open item becomes claimable again; needs `:base` and `:cap`.
const READY_AT: &str =
    "(last_attempt_ms + MIN(:base * (1 << MIN(MAX(attempts - 1, 0), 30)), :cap))";

const COLUMNS: &str = "id, kind, gap_key, query, intent, confidence, details, hits, status, \
                       first_seen_ms, last_seen_ms, updated_ms, claimed_ms, attempts, \
                       last_attempt_ms, last_error, resolution_note, closed_ms";

string_enum! {
    /// What sort of gap was found.
    pub enum GapKind {
        /// A query the engine answered with low confidence.
        LowConfidence = 1 => "low_confidence",
        /// A node that lacks an expected relation (for example an API without an owner).
        MissingNeighbor = 2 => "missing_neighbor",
        /// A mention in a query that matched nothing in the graph.
        UnresolvedEntity = 3 => "unresolved_entity",
        /// Authoritative and reference sources disagree.
        Conflict = 4 => "conflict",
    }
}

impl GapKind {
    /// Weight of the kind in the priority formula.
    pub const fn severity(self) -> f64 {
        match self {
            Self::Conflict => 3.0,
            Self::MissingNeighbor => 2.0,
            Self::UnresolvedEntity => 1.5,
            Self::LowConfidence => 1.0,
        }
    }
}

string_enum! {
    /// Where an item is in its lifecycle.
    pub enum GapStatus {
        /// Waiting for expansion.
        Open = 1 => "open",
        /// Claimed by a worker.
        InProgress = 2 => "in_progress",
        /// Expansion filled the gap.
        Resolved = 3 => "resolved",
        /// Deliberately ignored.
        Dismissed = 4 => "dismissed",
    }
}

/// A gap observed by the engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GapReport {
    /// A query answered with low confidence.
    LowConfidence {
        /// The user's query.
        query: String,
        /// How the query was understood.
        intent: QueryIntent,
        /// Confidence of the answer in `[0, 1]`.
        confidence: f32,
        /// Keys of what was retrieved.
        retrieved: Vec<String>,
    },
    /// A node without an expected relation.
    MissingNeighbor {
        /// Key of the node.
        node_key: String,
        /// The missing relation ("owned_by", "implements", ...).
        relation: String,
        /// Free-form context.
        note: Option<String>,
    },
    /// A mention that matched nothing.
    UnresolvedEntity {
        /// The mention as written.
        mention: String,
        /// The query it appeared in.
        query: Option<String>,
    },
    /// Authoritative and reference sources disagree.
    Conflict {
        /// Key of the node the sources disagree about.
        node_key: String,
        /// What they disagree about (an attribute or relation name).
        topic: String,
        /// What the authoritative source says.
        authoritative: String,
        /// What the reference source says.
        reference: String,
    },
}

struct Parts {
    kind: GapKind,
    key: String,
    query: Option<String>,
    intent: Option<String>,
    confidence: Option<f64>,
    details: String,
}

/// Lower-cases and collapses whitespace.
fn norm(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

impl GapReport {
    /// The kind of gap.
    pub fn kind(&self) -> GapKind {
        match self {
            Self::LowConfidence { .. } => GapKind::LowConfidence,
            Self::MissingNeighbor { .. } => GapKind::MissingNeighbor,
            Self::UnresolvedEntity { .. } => GapKind::UnresolvedEntity,
            Self::Conflict { .. } => GapKind::Conflict,
        }
    }

    /// The normalised deduplication key.
    pub fn gap_key(&self) -> String {
        match self {
            Self::LowConfidence { query, .. } => format!("low_confidence:{}", norm(query)),
            Self::MissingNeighbor {
                node_key, relation, ..
            } => format!("missing_neighbor:{}|{}", node_key.trim(), norm(relation)),
            Self::UnresolvedEntity { mention, .. } => {
                format!("unresolved_entity:{}", norm(mention))
            }
            Self::Conflict {
                node_key, topic, ..
            } => format!("conflict:{}|{}", node_key.trim(), norm(topic)),
        }
    }

    fn parts(&self) -> Result<Parts> {
        let (query, intent, confidence, details) = match self {
            Self::LowConfidence {
                query,
                intent,
                confidence,
                retrieved,
            } => (
                Some(query.clone()),
                Some(serde_json::to_string(intent)?),
                Some(f64::from(*confidence).clamp(0.0, 1.0)),
                json!({ "retrieved": retrieved }),
            ),
            Self::MissingNeighbor {
                node_key,
                relation,
                note,
            } => (
                None,
                None,
                None,
                json!({ "node_key": node_key, "relation": relation, "note": note }),
            ),
            Self::UnresolvedEntity { mention, query } => {
                (query.clone(), None, None, json!({ "mention": mention }))
            }
            Self::Conflict {
                node_key,
                topic,
                authoritative,
                reference,
            } => (
                None,
                None,
                None,
                json!({
                    "node_key": node_key,
                    "topic": topic,
                    "authoritative": authoritative,
                    "reference": reference,
                }),
            ),
        };
        Ok(Parts {
            kind: self.kind(),
            key: self.gap_key(),
            query,
            intent,
            confidence,
            details: serde_json::to_string(&details)?,
        })
    }
}

/// One row of the backlog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BacklogItem {
    /// Row id.
    pub id: i64,
    /// Kind of gap.
    pub kind: GapKind,
    /// Normalised deduplication key.
    pub gap_key: String,
    /// The query that exposed the gap, if any.
    pub query: Option<String>,
    /// The query intent as JSON, if any.
    pub intent: Option<serde_json::Value>,
    /// Lowest confidence seen, if the report carried one.
    pub confidence: Option<f64>,
    /// Kind-specific details.
    pub details: serde_json::Value,
    /// Number of times the gap was reported.
    pub hits: u64,
    /// Effective priority at the time the item was read (see the module documentation).
    pub priority: f64,
    /// Lifecycle state.
    pub status: GapStatus,
    /// First report, milliseconds since the Unix epoch.
    pub first_seen_ms: i64,
    /// Latest report.
    pub last_seen_ms: i64,
    /// Last change of any field.
    pub updated_ms: i64,
    /// When the item was last claimed.
    pub claimed_ms: Option<i64>,
    /// Number of claims so far.
    pub attempts: u32,
    /// When the item was last claimed (backoff reference).
    pub last_attempt_ms: Option<i64>,
    /// Reason of the last failed or deferred attempt.
    pub last_error: Option<String>,
    /// Note given when resolving or dismissing.
    pub resolution_note: Option<String>,
    /// When the item was resolved or dismissed.
    pub closed_ms: Option<i64>,
}

/// Sort key of [`Backlog::list`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    /// Effective priority.
    #[default]
    Priority,
    /// Latest report.
    LastSeen,
    /// First report.
    FirstSeen,
    /// Number of hits.
    Hits,
}

/// Filter, order and window of [`Backlog::list`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListFilter {
    /// Only items in this status.
    pub status: Option<GapStatus>,
    /// Only items of this kind.
    pub kind: Option<GapKind>,
    /// Only items whose query or gap key contains this text (case-insensitive).
    pub text: Option<String>,
    /// Sort key.
    pub sort: SortKey,
    /// Descending order (the default).
    pub descending: bool,
    /// Maximum rows (default 100).
    pub limit: usize,
    /// Rows to skip.
    pub offset: usize,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self {
            status: None,
            kind: None,
            text: None,
            sort: SortKey::Priority,
            descending: true,
            limit: 100,
            offset: 0,
        }
    }
}

/// Aggregate view of the backlog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklogStats {
    /// All rows.
    pub total: u64,
    /// Rows waiting for expansion.
    pub open: u64,
    /// Rows claimed by a worker.
    pub in_progress: u64,
    /// Resolved rows.
    pub resolved: u64,
    /// Dismissed rows.
    pub dismissed: u64,
    /// Open and in-progress rows per kind.
    pub open_by_kind: BTreeMap<GapKind, u64>,
    /// Sum of hits over all rows.
    pub total_hits: u64,
    /// First-seen time of the oldest open row.
    pub oldest_open_ms: Option<i64>,
    /// Open rows evicted by the size limit since this handle was created.
    pub evicted: u64,
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn bad_value(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, message.into())
}

fn kind_from(index: usize, value: i64) -> rusqlite::Result<GapKind> {
    u8::try_from(value)
        .ok()
        .and_then(GapKind::from_code)
        .ok_or_else(|| bad_value(index, format!("unknown gap kind {value}")))
}

fn status_from(index: usize, value: i64) -> rusqlite::Result<GapStatus> {
    u8::try_from(value)
        .ok()
        .and_then(GapStatus::from_code)
        .ok_or_else(|| bad_value(index, format!("unknown gap status {value}")))
}

fn json_from(index: usize, text: &str) -> rusqlite::Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|err| bad_value(index, err.to_string()))
}

fn base_priority(kind: GapKind, hits: i64, confidence: Option<f64>) -> f64 {
    let hits = f64::from(u32::try_from(hits.max(1)).unwrap_or(u32::MAX));
    kind.severity() * (1.0 + hits.ln()) * (2.0 - confidence.unwrap_or(0.5).clamp(0.0, 1.0))
}

fn item_from_row(row: &Row<'_>, now: i64) -> rusqlite::Result<BacklogItem> {
    let intent: Option<String> = row.get(4)?;
    let details: String = row.get(6)?;
    let first_seen_ms: i64 = row.get(9)?;
    let kind = kind_from(1, row.get(1)?)?;
    let hits: i64 = row.get(7)?;
    let confidence: Option<f64> = row.get(5)?;
    let last_seen_ms: i64 = row.get(10)?;
    let age = now.saturating_sub(last_seen_ms).max(0);
    let age = f64::from(u32::try_from(age / 1000).unwrap_or(u32::MAX)) * 1000.0;
    let priority = base_priority(kind, hits, confidence) / (1.0 + age / RECENCY_SCALE_MS);
    Ok(BacklogItem {
        id: row.get(0)?,
        kind,
        gap_key: row.get(2)?,
        query: row.get(3)?,
        intent: intent.map(|text| json_from(4, &text)).transpose()?,
        confidence,
        details: json_from(6, &details)?,
        hits: unsigned(hits),
        priority,
        status: status_from(8, row.get(8)?)?,
        first_seen_ms,
        last_seen_ms,
        updated_ms: row.get(11)?,
        claimed_ms: row.get(12)?,
        attempts: u32::try_from(row.get::<_, i64>(13)?).unwrap_or(u32::MAX),
        last_attempt_ms: row.get(14)?,
        last_error: row.get(15)?,
        resolution_note: row.get(16)?,
        closed_ms: row.get(17)?,
    })
}

fn fetch(conn: &Connection, id: i64, now: i64) -> Result<Option<BacklogItem>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM backlog_items WHERE id = ?1"),
            [id],
            |row| item_from_row(row, now),
        )
        .optional()?)
}

/// The gap-analysis backlog on SQLite. Cheap to clone; all methods are blocking (from async code
/// use `tokio::task::spawn_blocking`).
#[derive(Clone)]
pub struct Backlog {
    db: Db,
    max_open: usize,
    backoff_base_ms: i64,
    backoff_cap_ms: i64,
    max_attempts: u32,
    notify: Arc<Notify>,
    evicted: Arc<AtomicU64>,
}

impl std::fmt::Debug for Backlog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backlog")
            .field("max_open", &self.max_open)
            .finish_non_exhaustive()
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX / 4)
}

impl Backlog {
    /// A backlog over `db` (whose schema must include [`super::MIGRATIONS`]).
    pub fn new(db: Db, config: &BacklogConfig) -> Self {
        Self {
            db,
            max_open: config.max_open_items,
            backoff_base_ms: millis(DEFAULT_BACKOFF_BASE),
            backoff_cap_ms: millis(DEFAULT_BACKOFF_CAP),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            notify: Arc::new(Notify::new()),
            evicted: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Overrides the retry backoff: `min(base * 2^(attempts - 1), cap)`.
    #[must_use]
    pub fn with_backoff(mut self, base: Duration, cap: Duration) -> Self {
        self.backoff_base_ms = millis(base);
        self.backoff_cap_ms = millis(cap).max(self.backoff_base_ms);
        self
    }

    /// Overrides the number of attempts after which [`Self::fail`] dismisses an item.
    #[must_use]
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// Notified whenever an item is recorded or becomes open again; the worker waits on it.
    pub fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Records a gap and returns the id of its row. A repeat of a known gap increments `hits`,
    /// refreshes `last_seen_ms` and keeps the lowest confidence (see the module documentation for
    /// how a repeat treats resolved and dismissed items). Open items beyond the configured limit
    /// are evicted, lowest priority first.
    pub fn record(&self, report: &GapReport) -> Result<i64> {
        self.record_at(report, now_ms())
    }

    /// [`Self::record`] with an explicit clock, for tests.
    pub fn record_at(&self, report: &GapReport, now: i64) -> Result<i64> {
        let parts = report.parts()?;
        let (id, evicted) = self.db.transaction(|tx| {
            let existing = tx
                .query_row(
                    "SELECT id, hits, confidence, status, intent, query, details
                       FROM backlog_items WHERE gap_key = ?1",
                    [&parts.key],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<f64>>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let id = match existing {
                None => {
                    tx.execute(
                        "INSERT INTO backlog_items
                            (kind, gap_key, query, intent, confidence, details, hits, base_priority,
                             status, first_seen_ms, last_seen_ms, updated_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, 1, ?8, ?8, ?8)",
                        rusqlite::params![
                            i64::from(parts.kind.code()),
                            parts.key,
                            parts.query,
                            parts.intent,
                            parts.confidence,
                            parts.details,
                            base_priority(parts.kind, 1, parts.confidence),
                            now,
                        ],
                    )?;
                    tx.last_insert_rowid()
                }
                Some((id, hits, old_conf, status)) => {
                    let hits = hits.saturating_add(1);
                    let lower = match (parts.confidence, old_conf) {
                        (Some(new), Some(old)) => new <= old,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    let confidence = match (parts.confidence, old_conf) {
                        (Some(new), Some(old)) => Some(new.min(old)),
                        (new, old) => new.or(old),
                    };
                    let status = status_from(0, status)?;
                    let reopened = status == GapStatus::Resolved;
                    tx.execute(
                        "UPDATE backlog_items SET
                            hits = ?2, last_seen_ms = ?3, updated_ms = ?3, confidence = ?4,
                            base_priority = ?5,
                            query = CASE WHEN ?6 THEN ?7 ELSE query END,
                            intent = CASE WHEN ?6 THEN ?8 ELSE intent END,
                            details = CASE WHEN ?6 THEN ?9 ELSE details END,
                            status = CASE WHEN ?10 THEN 1 ELSE status END,
                            attempts = CASE WHEN ?10 THEN 0 ELSE attempts END,
                            last_attempt_ms = CASE WHEN ?10 THEN NULL ELSE last_attempt_ms END,
                            closed_ms = CASE WHEN ?10 THEN NULL ELSE closed_ms END
                         WHERE id = ?1",
                        rusqlite::params![
                            id,
                            hits,
                            now,
                            confidence,
                            base_priority(parts.kind, hits, confidence),
                            lower,
                            parts.query,
                            parts.intent,
                            parts.details,
                            reopened,
                        ],
                    )?;
                    id
                }
            };
            let evicted = self.evict_in(tx, now)?;
            Ok((id, evicted))
        })?;
        if evicted > 0 {
            self.evicted.fetch_add(evicted, Ordering::Relaxed);
        }
        self.notify.notify_one();
        Ok(id)
    }

    /// Deletes the lowest-priority open items beyond the size limit; returns how many.
    fn evict_in(&self, tx: &Transaction<'_>, now: i64) -> Result<u64> {
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM backlog_items WHERE status = 1",
            [],
            |row| row.get(0),
        )?;
        let limit = i64::try_from(self.max_open).unwrap_or(i64::MAX);
        let excess = open.saturating_sub(limit);
        if excess <= 0 {
            return Ok(0);
        }
        let removed = tx.execute(
            &format!(
                "DELETE FROM backlog_items WHERE id IN (
                     SELECT id FROM backlog_items WHERE status = 1
                     ORDER BY {EFFECTIVE} ASC, id ASC LIMIT :excess)"
            ),
            named_params! { ":now": now, ":excess": excess },
        )?;
        Ok(removed as u64)
    }

    /// Returns one item.
    pub fn get(&self, id: i64) -> Result<Option<BacklogItem>> {
        let now = now_ms();
        self.db.read(|conn| fetch(conn, id, now))
    }

    /// Lists items matching `filter`.
    pub fn list(&self, filter: &ListFilter) -> Result<Vec<BacklogItem>> {
        let now = now_ms();
        let mut clauses = vec![":now IS NOT NULL".to_owned()];
        let mut params: Vec<(&str, Value)> = vec![(":now", Value::Integer(now))];
        if let Some(status) = filter.status {
            clauses.push("status = :status".into());
            params.push((":status", Value::Integer(i64::from(status.code()))));
        }
        if let Some(kind) = filter.kind {
            clauses.push("kind = :kind".into());
            params.push((":kind", Value::Integer(i64::from(kind.code()))));
        }
        if let Some(text) = filter
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            clauses.push("instr(lower(COALESCE(query, '') || ' ' || gap_key), :text) > 0".into());
            params.push((":text", Value::Text(text.to_lowercase())));
        }
        let key = match filter.sort {
            SortKey::Priority => EFFECTIVE,
            SortKey::LastSeen => "last_seen_ms",
            SortKey::FirstSeen => "first_seen_ms",
            SortKey::Hits => "hits",
        };
        let direction = if filter.descending { "DESC" } else { "ASC" };
        let sql = format!(
            "SELECT {COLUMNS} FROM backlog_items WHERE {} ORDER BY {key} {direction}, id ASC
             LIMIT :limit OFFSET :offset",
            clauses.join(" AND ")
        );
        params.push((
            ":limit",
            Value::Integer(i64::try_from(filter.limit).unwrap_or(i64::MAX)),
        ));
        params.push((
            ":offset",
            Value::Integer(i64::try_from(filter.offset).unwrap_or(i64::MAX)),
        ));
        let refs: Vec<(&str, &dyn ToSql)> =
            params.iter().map(|(k, v)| (*k, v as &dyn ToSql)).collect();
        self.db.read(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(refs.as_slice(), |row| item_from_row(row, now))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Atomically claims the best open item that is not in backoff, marking it in progress and
    /// counting an attempt. `None` when nothing is claimable.
    pub fn claim_next(&self) -> Result<Option<BacklogItem>> {
        self.claim_next_at(now_ms())
    }

    /// [`Self::claim_next`] with an explicit clock, for tests.
    pub fn claim_next_at(&self, now: i64) -> Result<Option<BacklogItem>> {
        self.db.transaction(|tx| {
            let id: Option<i64> = tx
                .query_row(
                    &format!(
                        "SELECT id FROM backlog_items
                          WHERE status = 1 AND (last_attempt_ms IS NULL OR {READY_AT} <= :now)
                          ORDER BY {EFFECTIVE} DESC, id ASC LIMIT 1"
                    ),
                    named_params! {
                        ":now": now,
                        ":base": self.backoff_base_ms,
                        ":cap": self.backoff_cap_ms,
                    },
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id) = id else { return Ok(None) };
            tx.execute(
                "UPDATE backlog_items SET status = 2, claimed_ms = ?2, last_attempt_ms = ?2,
                        updated_ms = ?2, attempts = attempts + 1
                  WHERE id = ?1",
                rusqlite::params![id, now],
            )?;
            fetch(tx, id, now)
        })
    }

    /// How long until the earliest open item leaves its backoff (zero when one is claimable
    /// already); `None` when there are no open items.
    pub fn next_ready_in(&self) -> Result<Option<Duration>> {
        let now = now_ms();
        let at: Option<i64> = self.db.read(|conn| {
            Ok(conn.query_row(
                &format!("SELECT MIN(COALESCE({READY_AT}, 0)) FROM backlog_items WHERE status = 1"),
                named_params! { ":base": self.backoff_base_ms, ":cap": self.backoff_cap_ms },
                |row| row.get(0),
            )?)
        })?;
        Ok(at.map(|at| Duration::from_millis(unsigned(at.saturating_sub(now)))))
    }

    /// Runs `change` on an item if its status is one of `allowed`.
    fn apply<T>(
        &self,
        id: i64,
        op: &str,
        allowed: &[GapStatus],
        change: impl FnOnce(&Transaction<'_>, u32) -> Result<T>,
    ) -> Result<T> {
        let value = self.db.transaction(|tx| {
            let row: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT status, attempts FROM backlog_items WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((status, attempts)) = row else {
                return Err(Error::not_found(format!("backlog item {id}")));
            };
            let status = status_from(0, status)?;
            if !allowed.contains(&status) {
                return Err(Error::invalid(format!(
                    "cannot {op} backlog item {id}: it is {status}"
                )));
            }
            change(tx, u32::try_from(attempts).unwrap_or(u32::MAX))
        })?;
        self.notify.notify_one();
        Ok(value)
    }

    fn close(&self, id: i64, op: &str, to: GapStatus, note: &str) -> Result<()> {
        let now = now_ms();
        self.apply(
            id,
            op,
            &[GapStatus::Open, GapStatus::InProgress],
            |tx, _| {
                tx.execute(
                    "UPDATE backlog_items SET status = ?2, resolution_note = ?3, closed_ms = ?4,
                            updated_ms = ?4 WHERE id = ?1",
                    rusqlite::params![id, i64::from(to.code()), note, now],
                )?;
                Ok(())
            },
        )
    }

    /// Marks an open or in-progress item resolved.
    pub fn resolve(&self, id: i64, note: &str) -> Result<()> {
        self.close(id, "resolve", GapStatus::Resolved, note)
    }

    /// Marks an open or in-progress item dismissed.
    pub fn dismiss(&self, id: i64, note: &str) -> Result<()> {
        self.close(id, "dismiss", GapStatus::Dismissed, note)
    }

    /// Reopens a resolved or dismissed item with a fresh attempt count.
    pub fn reopen(&self, id: i64) -> Result<()> {
        let now = now_ms();
        self.apply(
            id,
            "reopen",
            &[GapStatus::Resolved, GapStatus::Dismissed],
            |tx, _| {
                tx.execute(
                    "UPDATE backlog_items SET status = 1, attempts = 0, last_attempt_ms = NULL,
                            last_error = NULL, claimed_ms = NULL, closed_ms = NULL, updated_ms = ?2
                      WHERE id = ?1",
                    rusqlite::params![id, now],
                )?;
                Ok(())
            },
        )
    }

    /// Makes an open or in-progress item claimable immediately, clearing its backoff.
    pub fn retry(&self, id: i64) -> Result<()> {
        let now = now_ms();
        self.apply(
            id,
            "retry",
            &[GapStatus::Open, GapStatus::InProgress],
            |tx, _| {
                tx.execute(
                    "UPDATE backlog_items SET status = 1, last_attempt_ms = NULL, claimed_ms = NULL,
                            updated_ms = ?2 WHERE id = ?1",
                    rusqlite::params![id, now],
                )?;
                Ok(())
            },
        )
    }

    /// Returns an in-progress item to open after an attempt that should be retried later; the
    /// attempt counts and the backoff applies.
    pub fn defer(&self, id: i64, reason: &str) -> Result<()> {
        let now = now_ms();
        self.apply(id, "defer", &[GapStatus::InProgress], |tx, _| {
            tx.execute(
                "UPDATE backlog_items SET status = 1, last_error = ?2, claimed_ms = NULL,
                        updated_ms = ?3 WHERE id = ?1",
                rusqlite::params![id, reason, now],
            )?;
            Ok(())
        })
    }

    /// Records a failed attempt of an in-progress item: back to open with the backoff applied, or
    /// dismissed once it has used up its attempts. Returns the resulting status.
    pub fn fail(&self, id: i64, reason: &str) -> Result<GapStatus> {
        let now = now_ms();
        let max = self.max_attempts;
        self.apply(id, "fail", &[GapStatus::InProgress], |tx, attempts| {
            if attempts >= max {
                tx.execute(
                    "UPDATE backlog_items SET status = 4, last_error = ?2, resolution_note = ?3,
                            closed_ms = ?4, updated_ms = ?4 WHERE id = ?1",
                    rusqlite::params![
                        id,
                        reason,
                        format!("gave up after {attempts} attempts: {reason}"),
                        now
                    ],
                )?;
                Ok(GapStatus::Dismissed)
            } else {
                tx.execute(
                    "UPDATE backlog_items SET status = 1, last_error = ?2, claimed_ms = NULL,
                            updated_ms = ?3 WHERE id = ?1",
                    rusqlite::params![id, reason, now],
                )?;
                Ok(GapStatus::Open)
            }
        })
    }

    /// Gives a claimed item back without penalty (the attempt is not counted), for example when
    /// the worker shuts down mid-expansion.
    pub fn release(&self, id: i64) -> Result<()> {
        let now = now_ms();
        self.apply(id, "release", &[GapStatus::InProgress], |tx, _| {
            tx.execute(
                "UPDATE backlog_items SET status = 1, attempts = MAX(attempts - 1, 0),
                        last_attempt_ms = CASE WHEN attempts <= 1 THEN NULL ELSE last_attempt_ms END,
                        claimed_ms = NULL, updated_ms = ?2 WHERE id = ?1",
                rusqlite::params![id, now],
            )?;
            Ok(())
        })
    }

    /// Crash recovery: returns every in-progress item to open (without counting the interrupted
    /// attempt). Call at start-up, before a worker runs. Returns the number of items recovered.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let now = now_ms();
        let count = self.db.write(|conn| {
            Ok(conn.execute(
                "UPDATE backlog_items SET status = 1, attempts = MAX(attempts - 1, 0),
                        last_attempt_ms = CASE WHEN attempts <= 1 THEN NULL ELSE last_attempt_ms END,
                        last_error = 'interrupted', claimed_ms = NULL, updated_ms = ?1
                  WHERE status = 2",
                [now],
            )?)
        })?;
        if count > 0 {
            self.notify.notify_one();
        }
        Ok(count)
    }

    /// Deletes resolved and dismissed items closed longer than `older_than` ago; returns how many.
    pub fn prune_closed(&self, older_than: Duration) -> Result<usize> {
        let cutoff = now_ms().saturating_sub(millis(older_than));
        self.db.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM backlog_items WHERE status IN (3, 4) AND COALESCE(closed_ms, updated_ms) < ?1",
                [cutoff],
            )?)
        })
    }

    /// Counts per status and kind.
    pub fn stats(&self) -> Result<BacklogStats> {
        let mut stats = BacklogStats {
            evicted: self.evicted.load(Ordering::Relaxed),
            ..BacklogStats::default()
        };
        self.db.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT status, kind, COUNT(*), SUM(hits) FROM backlog_items GROUP BY status, kind",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    status_from(0, row.get(0)?)?,
                    kind_from(1, row.get(1)?)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (status, kind, count, hits) = row?;
                let count = unsigned(count);
                stats.total += count;
                stats.total_hits += unsigned(hits);
                match status {
                    GapStatus::Open => stats.open += count,
                    GapStatus::InProgress => stats.in_progress += count,
                    GapStatus::Resolved => stats.resolved += count,
                    GapStatus::Dismissed => stats.dismissed += count,
                }
                if matches!(status, GapStatus::Open | GapStatus::InProgress) {
                    *stats.open_by_kind.entry(kind).or_default() += count;
                }
            }
            stats.oldest_open_ms = conn.query_row(
                "SELECT MIN(first_seen_ms) FROM backlog_items WHERE status = 1",
                [],
                |row| row.get(0),
            )?;
            Ok(())
        })?;
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{ClassifierKind, Complexity, IntentKind, MigrationSet};

    const SCHEMA: &[MigrationSet] = &[MigrationSet {
        module: "backlog",
        migrations: crate::backlog::MIGRATIONS,
    }];

    fn backlog_with(max_open: usize) -> Backlog {
        let db = Db::open_in_memory_with(SCHEMA).expect("db");
        let config = BacklogConfig {
            max_open_items: max_open,
            ..BacklogConfig::default()
        };
        Backlog::new(db, &config)
    }

    fn backlog() -> Backlog {
        backlog_with(5000)
    }

    fn low(query: &str, confidence: f32) -> GapReport {
        GapReport::LowConfidence {
            query: query.to_owned(),
            intent: QueryIntent {
                kind: IntentKind::Ownership,
                entities: Vec::new(),
                target_kinds: Vec::new(),
                complexity: Complexity::Simple,
                confidence,
                classifier: ClassifierKind::Heuristic,
            },
            confidence,
            retrieved: vec!["api:payments".into()],
        }
    }

    fn missing(node: &str) -> GapReport {
        GapReport::MissingNeighbor {
            node_key: node.to_owned(),
            relation: "owned_by".into(),
            note: None,
        }
    }

    fn item(b: &Backlog, id: i64) -> BacklogItem {
        b.get(id).expect("get").expect("present")
    }

    #[test]
    fn repeats_dedupe_count_hits_and_keep_lowest_confidence() {
        let b = backlog();
        let a = b
            .record_at(&low("Who owns  Payments?", 0.6), 1_000)
            .unwrap();
        let again = b.record_at(&low("who owns payments?", 0.2), 2_000).unwrap();
        let third = b.record_at(&low("WHO OWNS PAYMENTS?", 0.9), 3_000).unwrap();
        assert_eq!((a, again, third), (a, a, a));
        let it = item(&b, a);
        assert_eq!(it.hits, 3);
        assert_eq!(it.first_seen_ms, 1_000);
        assert_eq!(it.last_seen_ms, 3_000);
        assert!((it.confidence.unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(b.stats().unwrap().total, 1);
        // A different kind with similar text is a different gap.
        let other = b
            .record(&GapReport::UnresolvedEntity {
                mention: "who owns payments?".into(),
                query: None,
            })
            .unwrap();
        assert_ne!(other, a);
    }

    #[test]
    fn gap_keys_are_normalised() {
        assert_eq!(
            missing("api:x").gap_key(),
            "missing_neighbor:api:x|owned_by"
        );
        let conflict = GapReport::Conflict {
            node_key: "api:x".into(),
            topic: " Owner  Team ".into(),
            authoritative: "a".into(),
            reference: "b".into(),
        };
        assert_eq!(conflict.gap_key(), "conflict:api:x|owner team");
    }

    #[test]
    fn priority_orders_by_kind_hits_confidence_and_recency() {
        let b = backlog();
        let now = now_ms();
        let plain = b.record_at(&low("plain", 0.9), now).unwrap();
        let uncertain = b.record_at(&low("uncertain", 0.1), now).unwrap();
        let frequent = b.record_at(&low("frequent", 0.9), now).unwrap();
        for _ in 0..9 {
            b.record_at(&low("frequent", 0.9), now).unwrap();
        }
        let gap = b.record_at(&missing("api:x"), now).unwrap();
        let conflict = b
            .record_at(
                &GapReport::Conflict {
                    node_key: "api:x".into(),
                    topic: "owner".into(),
                    authoritative: "a".into(),
                    reference: "b".into(),
                },
                now,
            )
            .unwrap();
        let stale = b
            .record_at(&low("stale", 0.1), now - 28 * 86_400_000)
            .unwrap();
        let order: Vec<i64> = b
            .list(&ListFilter::default())
            .unwrap()
            .iter()
            .map(|i| i.id)
            .collect();
        // frequent: 1*(1+ln 10)*1.1 = 3.63; conflict 3*1*1.5 = 4.5; gap 2*1*1.5 = 3.0;
        // uncertain 1.9; plain 1.1; stale 1.9/5 = 0.38.
        assert_eq!(
            order,
            vec![conflict, frequent, gap, uncertain, plain, stale]
        );
    }

    #[test]
    fn claim_takes_best_first_and_marks_in_progress() {
        let b = backlog();
        let low_id = b.record(&low("minor", 0.9)).unwrap();
        let high_id = b.record(&missing("api:x")).unwrap();
        let first = b.claim_next().unwrap().unwrap();
        assert_eq!(first.id, high_id);
        assert_eq!(first.status, GapStatus::InProgress);
        assert_eq!(first.attempts, 1);
        let second = b.claim_next().unwrap().unwrap();
        assert_eq!(second.id, low_id);
        assert!(b.claim_next().unwrap().is_none());
    }

    #[test]
    fn claim_skips_items_in_backoff_exponentially() {
        let b = backlog().with_backoff(Duration::from_millis(1_000), Duration::from_millis(5_000));
        let id = b.record_at(&missing("api:x"), 0).unwrap();
        let t0 = 10_000;
        assert_eq!(b.claim_next_at(t0).unwrap().unwrap().id, id);
        b.defer(id, "later").unwrap();
        // attempts = 1: backoff 1s.
        assert!(b.claim_next_at(t0 + 999).unwrap().is_none());
        assert_eq!(b.claim_next_at(t0 + 1_000).unwrap().unwrap().attempts, 2);
        b.defer(id, "later").unwrap();
        // attempts = 2: backoff 2s measured from the second claim.
        assert!(b.claim_next_at(t0 + 1_000 + 1_999).unwrap().is_none());
        assert!(b.claim_next_at(t0 + 1_000 + 2_000).unwrap().is_some());
        b.defer(id, "later").unwrap();
        // attempts = 3 -> 4s; 4 -> 8s capped to 5s.
        assert!(b.claim_next_at(t0 + 3_000 + 4_000).unwrap().is_some());
        b.defer(id, "later").unwrap();
        assert!(b.claim_next_at(t0 + 7_000 + 4_999).unwrap().is_none());
        assert!(b.claim_next_at(t0 + 7_000 + 5_000).unwrap().is_some());
        // Manual retry clears the backoff.
        b.defer(id, "later").unwrap();
        b.retry(id).unwrap();
        assert!(b.claim_next_at(t0 + 12_000).unwrap().is_some());
        // A fresh item is not held back by a backed-off one.
        let other = b.record_at(&missing("api:y"), 0).unwrap();
        b.defer(id, "again").unwrap();
        assert_eq!(b.claim_next_at(t0 + 12_001).unwrap().unwrap().id, other);
    }

    #[test]
    fn state_transitions_and_their_errors() {
        let b = backlog();
        let id = b.record(&missing("api:x")).unwrap();
        assert!(b.reopen(id).is_err());
        assert!(b.defer(id, "x").is_err());
        b.claim_next().unwrap().unwrap();
        b.resolve(id, "owner added").unwrap();
        let it = item(&b, id);
        assert_eq!(it.status, GapStatus::Resolved);
        assert_eq!(it.resolution_note.as_deref(), Some("owner added"));
        assert!(it.closed_ms.is_some());
        assert!(b.resolve(id, "again").is_err());
        assert!(b.dismiss(id, "no").is_err());
        b.reopen(id).unwrap();
        let it = item(&b, id);
        assert_eq!((it.status, it.attempts), (GapStatus::Open, 0));
        b.dismiss(id, "not relevant").unwrap();
        assert_eq!(item(&b, id).status, GapStatus::Dismissed);
        assert!(b.claim_next().unwrap().is_none());
        b.reopen(id).unwrap();
        assert!(b.claim_next().unwrap().is_some());
        assert!(matches!(b.resolve(999, ""), Err(Error::NotFound(_))));
    }

    #[test]
    fn repeat_of_resolved_reopens_and_of_dismissed_only_counts() {
        let b = backlog();
        let resolved = b.record(&missing("api:a")).unwrap();
        b.resolve(resolved, "done").unwrap();
        b.record(&missing("api:a")).unwrap();
        assert_eq!(item(&b, resolved).status, GapStatus::Open);
        let dismissed = b.record(&missing("api:b")).unwrap();
        b.dismiss(dismissed, "no").unwrap();
        b.record(&missing("api:b")).unwrap();
        let it = item(&b, dismissed);
        assert_eq!((it.status, it.hits), (GapStatus::Dismissed, 2));
    }

    #[test]
    fn fail_gives_up_after_max_attempts() {
        let b = backlog().with_max_attempts(2);
        let id = b.record(&missing("api:x")).unwrap();
        b.claim_next_at(0).unwrap().unwrap();
        assert_eq!(b.fail(id, "boom").unwrap(), GapStatus::Open);
        b.claim_next_at(i64::MAX / 8).unwrap().unwrap();
        assert_eq!(b.fail(id, "boom").unwrap(), GapStatus::Dismissed);
        let it = item(&b, id);
        assert!(it.resolution_note.unwrap().contains("gave up after 2"));
        assert_eq!(it.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn eviction_drops_lowest_priority_open_items_only() {
        let b = backlog_with(3);
        let keep_closed = b.record(&low("closed", 0.9)).unwrap();
        b.resolve(keep_closed, "done").unwrap();
        let ids: Vec<i64> = (0..3)
            .map(|n| b.record(&low(&format!("q{n}"), 0.9)).unwrap())
            .collect();
        let important = b.record(&missing("api:x")).unwrap();
        let open = b
            .list(&ListFilter {
                status: Some(GapStatus::Open),
                ..ListFilter::default()
            })
            .unwrap();
        assert_eq!(open.len(), 3);
        assert!(open.iter().any(|i| i.id == important));
        // Equal priorities: the oldest is evicted first.
        assert!(b.get(ids[0]).unwrap().is_none());
        assert!(b.get(keep_closed).unwrap().is_some());
        assert_eq!(b.stats().unwrap().evicted, 1);
    }

    #[test]
    fn crash_recovery_reopens_in_progress_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.db");
        let id = {
            let db = Db::open_with(&path, SCHEMA).unwrap();
            let b = Backlog::new(db, &BacklogConfig::default());
            let id = b.record(&missing("api:x")).unwrap();
            b.claim_next().unwrap().unwrap();
            id
            // "crash": the handle is dropped with the item in progress.
        };
        let db = Db::open_with(&path, SCHEMA).unwrap();
        let b = Backlog::new(db, &BacklogConfig::default());
        assert_eq!(item(&b, id).status, GapStatus::InProgress);
        assert!(b.claim_next().unwrap().is_none());
        assert_eq!(b.recover_interrupted().unwrap(), 1);
        let it = item(&b, id);
        assert_eq!((it.status, it.attempts), (GapStatus::Open, 0));
        // The interrupted attempt is not penalised with a backoff.
        assert_eq!(b.claim_next().unwrap().unwrap().id, id);
    }

    #[test]
    fn filters_sorting_paging_and_text_search() {
        let b = backlog();
        b.record_at(&low("alpha payments", 0.5), 1_000).unwrap();
        b.record_at(&low("beta billing", 0.5), 2_000).unwrap();
        b.record_at(&missing("api:payments"), 3_000).unwrap();
        let done = b.record_at(&low("gamma", 0.5), 4_000).unwrap();
        b.resolve(done, "ok").unwrap();

        let by_text = b
            .list(&ListFilter {
                text: Some("PAYMENTS".into()),
                ..ListFilter::default()
            })
            .unwrap();
        assert_eq!(by_text.len(), 2);
        let by_kind = b
            .list(&ListFilter {
                kind: Some(GapKind::MissingNeighbor),
                ..ListFilter::default()
            })
            .unwrap();
        assert_eq!(by_kind.len(), 1);
        let resolved = b
            .list(&ListFilter {
                status: Some(GapStatus::Resolved),
                ..ListFilter::default()
            })
            .unwrap();
        assert_eq!(resolved.len(), 1);
        let oldest_first = b
            .list(&ListFilter {
                sort: SortKey::FirstSeen,
                descending: false,
                limit: 2,
                offset: 1,
                ..ListFilter::default()
            })
            .unwrap();
        let queries: Vec<_> = oldest_first.iter().map(|i| i.gap_key.clone()).collect();
        assert_eq!(
            queries,
            vec![
                "low_confidence:beta billing".to_owned(),
                "missing_neighbor:api:payments|owned_by".to_owned()
            ]
        );
    }

    #[test]
    fn stats_and_prune() {
        let b = backlog();
        let a = b.record(&low("a", 0.5)).unwrap();
        b.record(&low("a", 0.5)).unwrap();
        let m = b.record(&missing("api:x")).unwrap();
        let c = b.record(&low("c", 0.5)).unwrap();
        b.claim_next().unwrap().unwrap();
        b.resolve(a, "r").unwrap();
        b.dismiss(c, "d").unwrap();
        let s = b.stats().unwrap();
        assert_eq!((s.total, s.resolved, s.dismissed), (3, 1, 1));
        assert_eq!(s.open + s.in_progress, 1);
        assert_eq!(s.total_hits, 4);
        assert_eq!(s.open_by_kind.get(&GapKind::MissingNeighbor), Some(&1));
        assert!(s.oldest_open_ms.is_some() || s.in_progress == 1);
        let _ = m;
        assert_eq!(b.prune_closed(Duration::from_secs(3600)).unwrap(), 0);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(b.prune_closed(Duration::ZERO).unwrap(), 2);
        assert_eq!(b.stats().unwrap().total, 1);
    }

    #[test]
    fn release_does_not_count_the_attempt() {
        let b = backlog();
        let id = b.record(&missing("api:x")).unwrap();
        b.claim_next().unwrap().unwrap();
        b.release(id).unwrap();
        let it = item(&b, id);
        assert_eq!(
            (it.status, it.attempts, it.last_attempt_ms),
            (GapStatus::Open, 0, None)
        );
    }
}
