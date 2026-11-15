//! The SQLite token ledger.
//!
//! [`TokenLedger::record`] prices the call and hands it to a dedicated writer thread through a
//! bounded queue, so callers never wait for the disk and can never be failed by it. The writer
//! batches whatever has queued up into one transaction. Reporting queries read committed rows:
//! call [`TokenLedger::flush`] first when the very latest calls must be visible.
//!
//! All reporting methods and `flush` block on SQLite; from async code run them on the blocking
//! pool (`TokenLedger` is cheap to clone into `tokio::task::spawn_blocking`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};

use crate::base::{
    CallPurpose, Db, Error, Locality, ModelTier, Result, UsageRecord, UsageSink, now_ms,
};

use super::pricing::{PriceTable, nanos_to_usd};

/// Default bound of the queue between callers and the writer thread.
pub const DEFAULT_QUEUE_CAPACITY: usize = 8192;

/// Rows written per transaction at most, so one burst cannot hold the single writer for long.
const MAX_BATCH: usize = 512;

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

/// One row of the ledger: a model call with its cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Row id (0 until the row is stored).
    pub id: i64,
    /// When the call finished, milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// Identifier of the user query the call belongs to, if any.
    pub query_id: Option<String>,
    /// Tier that served the call.
    pub tier: ModelTier,
    /// Local or cloud.
    pub locality: Locality,
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Why the call was made.
    pub purpose: CallPurpose,
    /// Prompt (input) tokens.
    pub prompt_tokens: u64,
    /// Completion (output) tokens.
    pub completion_tokens: u64,
    /// Cost incurred, nano-dollars (0 for local calls and unpriced models).
    pub cost_nanos: i64,
    /// What the same tokens cost on a metered cloud model, nano-dollars.
    pub cloud_equivalent_nanos: i64,
    /// Request latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Error text of a failed call.
    pub error: Option<String>,
    /// `true` when the token counts were estimated rather than reported by the provider.
    pub usage_estimated: bool,
    /// `false` when the model had no known price (the cost is then recorded as 0).
    pub priced: bool,
}

/// A half-open time range `[from_ms, to_ms)`; either bound may be absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    /// Inclusive lower bound, milliseconds since the Unix epoch.
    pub from_ms: Option<i64>,
    /// Exclusive upper bound, milliseconds since the Unix epoch.
    pub to_ms: Option<i64>,
}

impl TimeRange {
    /// Everything ever recorded.
    pub const fn all() -> Self {
        Self {
            from_ms: None,
            to_ms: None,
        }
    }

    /// From `from_ms` (inclusive) onwards.
    pub const fn since(from_ms: i64) -> Self {
        Self {
            from_ms: Some(from_ms),
            to_ms: None,
        }
    }

    /// From `from_ms` (inclusive) to `to_ms` (exclusive).
    pub const fn between(from_ms: i64, to_ms: i64) -> Self {
        Self {
            from_ms: Some(from_ms),
            to_ms: Some(to_ms),
        }
    }

    /// The last `window` up to now.
    pub fn last(window: Duration) -> Self {
        let window = i64::try_from(window.as_millis()).unwrap_or(i64::MAX);
        Self::since(now_ms().saturating_sub(window))
    }

    fn bounds(&self) -> (i64, i64) {
        (
            self.from_ms.unwrap_or(i64::MIN),
            self.to_ms.unwrap_or(i64::MAX),
        )
    }
}

/// Aggregated usage of a set of calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageStats {
    /// Number of calls, failures included.
    pub calls: u64,
    /// Number of failed calls.
    pub failures: u64,
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Cost incurred, nano-dollars.
    pub cost_nanos: i64,
    /// Cloud-equivalent cost, nano-dollars.
    pub cloud_equivalent_nanos: i64,
    /// Mean latency in milliseconds (0 without calls).
    pub mean_latency_ms: f64,
    /// Median latency (nearest rank), milliseconds.
    pub p50_latency_ms: u64,
    /// 95th percentile latency (nearest rank), milliseconds.
    pub p95_latency_ms: u64,
}

impl UsageStats {
    /// Prompt plus completion tokens.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Cloud-equivalent cost minus actual cost.
    pub fn savings_nanos(&self) -> i64 {
        self.cloud_equivalent_nanos.saturating_sub(self.cost_nanos)
    }
}

/// Usage over a time range, split by locality and by tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerSummary {
    /// The range that was summarised.
    pub range: TimeRange,
    /// All calls.
    pub total: UsageStats,
    /// Calls served locally.
    pub local: UsageStats,
    /// Calls served by a metered cloud API.
    pub cloud: UsageStats,
    /// Calls served by System 1.
    pub system1: UsageStats,
    /// Calls served by System 2.
    pub system2: UsageStats,
    /// Total savings: cloud-equivalent cost minus actual cost, nano-dollars.
    pub savings_nanos: i64,
}

impl LedgerSummary {
    /// Statistics for one locality.
    pub fn locality(&self, locality: Locality) -> &UsageStats {
        match locality {
            Locality::Local => &self.local,
            Locality::Cloud => &self.cloud,
        }
    }

    /// Statistics for one tier.
    pub fn tier(&self, tier: ModelTier) -> &UsageStats {
        match tier {
            ModelTier::System1 => &self.system1,
            ModelTier::System2 => &self.system2,
        }
    }
}

/// Width of a time-series bucket. Buckets are aligned to UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    /// One hour.
    Hour,
    /// One UTC day.
    Day,
}

impl Bucket {
    /// Bucket width in milliseconds.
    pub const fn width_ms(self) -> i64 {
        match self {
            Bucket::Hour => HOUR_MS,
            Bucket::Day => DAY_MS,
        }
    }

    /// Start of the bucket containing `timestamp_ms`.
    pub const fn start_of(self, timestamp_ms: i64) -> i64 {
        timestamp_ms - timestamp_ms.rem_euclid(self.width_ms())
    }
}

/// One non-empty bucket of a time series.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeBucket {
    /// Start of the bucket, milliseconds since the Unix epoch.
    pub start_ms: i64,
    /// Number of calls.
    pub calls: u64,
    /// Number of failed calls.
    pub failures: u64,
    /// Calls served locally.
    pub local_calls: u64,
    /// Calls served by the cloud.
    pub cloud_calls: u64,
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Cost incurred, nano-dollars.
    pub cost_nanos: i64,
    /// Cloud-equivalent cost, nano-dollars.
    pub cloud_equivalent_nanos: i64,
    /// Mean latency, milliseconds.
    pub mean_latency_ms: f64,
}

/// Usage of one `(provider, model, locality)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Local or cloud.
    pub locality: Locality,
    /// Number of calls.
    pub calls: u64,
    /// Number of failed calls.
    pub failures: u64,
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Cost incurred, nano-dollars.
    pub cost_nanos: i64,
    /// Cloud-equivalent cost, nano-dollars.
    pub cloud_equivalent_nanos: i64,
    /// Mean latency, milliseconds.
    pub mean_latency_ms: f64,
    /// `false` when at least one call of this model had no known price.
    pub priced: bool,
}

/// Nearest-rank percentile of an ascending slice: the smallest value such that at least
/// `percent` % of the values are less than or equal to it. Returns 0 for an empty slice;
/// `percent` is clamped to `[0, 100]`.
pub fn percentile(sorted: &[u64], percent: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let percent = if percent.is_nan() {
        0.0
    } else {
        percent.clamp(0.0, 100.0)
    };
    let rank = (percent / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

enum Message {
    Record(Box<LedgerEntry>),
    /// Reply with the first write error since the previous flush, if any.
    Flush(SyncSender<Option<String>>),
}

struct Inner {
    db: Db,
    prices: PriceTable,
    sender: Option<SyncSender<Message>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    accepted: AtomicU64,
    dropped: Arc<AtomicU64>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the channel lets the writer drain what is queued and exit.
        self.sender.take();
        if let Some(handle) = self.writer.lock().take()
            && handle.join().is_err()
        {
            tracing::error!("token ledger writer thread panicked");
        }
    }
}

/// SQLite-backed [`UsageSink`] with reporting queries. Cloning is cheap; the queue is flushed
/// when the last clone is dropped.
#[derive(Clone)]
pub struct TokenLedger {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TokenLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenLedger")
            .field("accepted", &self.accepted_records())
            .field("dropped", &self.dropped_records())
            .finish()
    }
}

impl TokenLedger {
    /// Creates a ledger on `db` (which must have the metrics schema applied) with the default
    /// queue capacity.
    pub fn new(db: Db, prices: PriceTable) -> Result<Self> {
        Self::with_capacity(db, prices, DEFAULT_QUEUE_CAPACITY)
    }

    /// Like [`Self::new`] with an explicit queue bound (at least 1).
    pub fn with_capacity(db: Db, prices: PriceTable, capacity: usize) -> Result<Self> {
        let (sender, receiver) = sync_channel(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer = std::thread::Builder::new()
            .name("anchor-ledger".into())
            .spawn({
                let db = db.clone();
                let dropped = Arc::clone(&dropped);
                move || writer_loop(&db, &receiver, &dropped)
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                prices,
                sender: Some(sender),
                writer: Mutex::new(Some(writer)),
                accepted: AtomicU64::new(0),
                dropped,
            }),
        })
    }

    /// The price table used to cost calls.
    pub fn prices(&self) -> &PriceTable {
        &self.inner.prices
    }

    /// Records accepted into the queue so far.
    pub fn accepted_records(&self) -> u64 {
        self.inner.accepted.load(Ordering::Relaxed)
    }

    /// Records lost so far: rejected because the queue was full, or accepted but not written
    /// because SQLite failed.
    pub fn dropped_records(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Blocks until every record queued before this call is committed. Fails when the writer
    /// is gone or when a batch since the previous flush could not be written.
    pub fn flush(&self) -> Result<()> {
        let sender = self.sender()?;
        let (ack, done) = sync_channel(1);
        sender
            .send(Message::Flush(ack))
            .map_err(|_| Error::other("token ledger writer has stopped"))?;
        match done.recv() {
            Ok(None) => Ok(()),
            Ok(Some(error)) => Err(Error::other(format!("token ledger write failed: {error}"))),
            Err(_) => Err(Error::other("token ledger writer has stopped")),
        }
    }

    fn sender(&self) -> Result<&SyncSender<Message>> {
        self.inner
            .sender
            .as_ref()
            .ok_or_else(|| Error::other("token ledger is shut down"))
    }

    /// Usage in `range`, split by locality and tier, with latency statistics.
    pub fn summary(&self, range: TimeRange) -> Result<LedgerSummary> {
        let (from, to) = range.bounds();
        self.inner.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT tier, locality, prompt_tokens, completion_tokens, cost_nanos,
                        cloud_equivalent_nanos, latency_ms, ok
                 FROM token_ledger WHERE ts_ms >= ?1 AND ts_ms < ?2",
            )?;
            let mut rows = statement.query(params![from, to])?;
            let mut total = Accumulator::default();
            let mut by_locality = [Accumulator::default(), Accumulator::default()];
            let mut by_tier = [Accumulator::default(), Accumulator::default()];
            while let Some(row) = rows.next()? {
                let tier = tier_from(row.get(0)?)?;
                let locality = locality_from(row.get(1)?)?;
                let sample = Sample {
                    prompt: unsigned(row.get(2)?),
                    completion: unsigned(row.get(3)?),
                    cost: row.get(4)?,
                    equivalent: row.get(5)?,
                    latency: unsigned(row.get(6)?),
                    ok: row.get(7)?,
                };
                total.add(&sample);
                by_locality[usize::from(locality == Locality::Cloud)].add(&sample);
                by_tier[usize::from(tier == ModelTier::System2)].add(&sample);
            }
            let [local, cloud] = by_locality.map(Accumulator::finish);
            let [system1, system2] = by_tier.map(Accumulator::finish);
            let total = total.finish();
            Ok(LedgerSummary {
                range,
                total,
                local,
                cloud,
                system1,
                system2,
                savings_nanos: total.savings_nanos(),
            })
        })
    }

    /// Cumulative totals over everything recorded.
    pub fn totals(&self) -> Result<LedgerSummary> {
        self.summary(TimeRange::all())
    }

    /// Usage per UTC-aligned bucket in `range`, oldest first. Empty buckets are omitted.
    pub fn timeseries(&self, bucket: Bucket, range: TimeRange) -> Result<Vec<TimeBucket>> {
        let (from, to) = range.bounds();
        let width = bucket.width_ms();
        self.inner.db.read(|conn| {
            // The double modulo floors towards negative infinity for pre-1970 timestamps.
            let mut statement = conn.prepare_cached(
                "SELECT ts_ms - ((ts_ms % ?3) + ?3) % ?3 AS bucket,
                        COUNT(*), SUM(ok = 0), SUM(locality = 1), SUM(locality = 2),
                        SUM(prompt_tokens), SUM(completion_tokens), SUM(cost_nanos),
                        SUM(cloud_equivalent_nanos), AVG(latency_ms)
                 FROM token_ledger WHERE ts_ms >= ?1 AND ts_ms < ?2
                 GROUP BY bucket ORDER BY bucket",
            )?;
            let rows = statement.query_map(params![from, to, width], |row| {
                Ok(TimeBucket {
                    start_ms: row.get(0)?,
                    calls: unsigned(row.get(1)?),
                    failures: unsigned(row.get(2)?),
                    local_calls: unsigned(row.get(3)?),
                    cloud_calls: unsigned(row.get(4)?),
                    prompt_tokens: unsigned(row.get(5)?),
                    completion_tokens: unsigned(row.get(6)?),
                    cost_nanos: row.get(7)?,
                    cloud_equivalent_nanos: row.get(8)?,
                    mean_latency_ms: row.get(9)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Usage per `(provider, model, locality)` in `range`, most expensive first (then by
    /// cloud-equivalent cost, then by calls).
    pub fn by_model(&self, range: TimeRange) -> Result<Vec<ModelUsage>> {
        let (from, to) = range.bounds();
        self.inner.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT provider, model, locality, COUNT(*), SUM(ok = 0), SUM(prompt_tokens),
                        SUM(completion_tokens), SUM(cost_nanos), SUM(cloud_equivalent_nanos),
                        AVG(latency_ms), MIN(priced)
                 FROM token_ledger WHERE ts_ms >= ?1 AND ts_ms < ?2
                 GROUP BY provider, model, locality
                 ORDER BY 8 DESC, 9 DESC, 4 DESC, provider, model",
            )?;
            let mut rows = statement.query(params![from, to])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(ModelUsage {
                    provider: row.get(0)?,
                    model: row.get(1)?,
                    locality: locality_from(row.get(2)?)?,
                    calls: unsigned(row.get(3)?),
                    failures: unsigned(row.get(4)?),
                    prompt_tokens: unsigned(row.get(5)?),
                    completion_tokens: unsigned(row.get(6)?),
                    cost_nanos: row.get(7)?,
                    cloud_equivalent_nanos: row.get(8)?,
                    mean_latency_ms: row.get(9)?,
                    priced: row.get(10)?,
                });
            }
            Ok(out)
        })
    }

    /// The most recent calls, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<LedgerEntry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.entries(
            "SELECT * FROM token_ledger ORDER BY ts_ms DESC, id DESC LIMIT ?1",
            move |statement| statement.query(params![limit]),
        )
    }

    /// Every call made on behalf of one user query, oldest first.
    pub fn for_query(&self, query_id: &str) -> Result<Vec<LedgerEntry>> {
        let query_id = query_id.to_owned();
        self.entries(
            "SELECT * FROM token_ledger WHERE query_id = ?1 ORDER BY ts_ms, id",
            move |statement| statement.query(params![query_id]),
        )
    }

    fn entries(
        &self,
        sql: &str,
        run: impl for<'s> FnOnce(
            &'s mut rusqlite::CachedStatement<'_>,
        ) -> rusqlite::Result<rusqlite::Rows<'s>>,
    ) -> Result<Vec<LedgerEntry>> {
        self.inner.db.read(|conn| {
            let mut statement = conn.prepare_cached(sql)?;
            let mut rows = run(&mut statement)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(entry_from_row(row)?);
            }
            Ok(out)
        })
    }

    /// Cloud spend from `from_ms` (inclusive) onwards, nano-dollars.
    pub fn cloud_spend_since_nanos(&self, from_ms: i64) -> Result<i64> {
        self.inner.db.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT COALESCE(SUM(cost_nanos), 0) FROM token_ledger
                     WHERE locality = 2 AND ts_ms >= ?1",
                )?
                .query_row(params![from_ms], |row| row.get(0))?)
        })
    }

    /// Cloud spend since the start of the current UTC day, nano-dollars. This is what the
    /// router's daily budget guard compares against; it only counts committed rows, so calls
    /// still in the queue (normally a few milliseconds' worth) are not yet included.
    pub fn cloud_spend_today_nanos(&self) -> Result<i64> {
        self.cloud_spend_since_nanos(Bucket::Day.start_of(now_ms()))
    }

    /// [`Self::cloud_spend_today_nanos`] in US dollars.
    pub fn cloud_spend_today_usd(&self) -> Result<f64> {
        self.cloud_spend_today_nanos().map(nanos_to_usd)
    }

    /// Deletes rows older than `older_than_ms` (exclusive) and returns how many were removed.
    /// Rows still queued are flushed first so the cut is consistent.
    pub fn prune(&self, older_than_ms: i64) -> Result<usize> {
        self.flush()?;
        self.inner.db.transaction(|tx| {
            Ok(tx.execute(
                "DELETE FROM token_ledger WHERE ts_ms < ?1",
                params![older_than_ms],
            )?)
        })
    }

    fn entry_for(&self, record: UsageRecord) -> LedgerEntry {
        let estimate = self.inner.prices.estimate(
            record.locality,
            &record.provider,
            &record.model,
            record.usage,
        );
        LedgerEntry {
            id: 0,
            timestamp_ms: record.timestamp_ms,
            query_id: record.query_id,
            tier: record.tier,
            locality: record.locality,
            provider: record.provider,
            model: record.model,
            purpose: record.purpose,
            prompt_tokens: record.usage.prompt_tokens,
            completion_tokens: record.usage.completion_tokens,
            cost_nanos: estimate.cost_nanos,
            cloud_equivalent_nanos: estimate.cloud_equivalent_nanos,
            latency_ms: record.latency_ms,
            ok: record.ok,
            error: record.error,
            usage_estimated: record.usage_estimated,
            priced: estimate.priced,
        }
    }
}

impl UsageSink for TokenLedger {
    fn record(&self, record: UsageRecord) {
        let entry = Box::new(self.entry_for(record));
        let sent = match &self.inner.sender {
            Some(sender) => match sender.try_send(Message::Record(entry)) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            },
            None => false,
        };
        let counter = if sent {
            &self.inner.accepted
        } else {
            &*self.inner.dropped
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn writer_loop(db: &Db, receiver: &Receiver<Message>, dropped: &AtomicU64) {
    let mut first_error: Option<String> = None;
    // `recv` parks the thread: an idle ledger costs no CPU.
    while let Ok(first) = receiver.recv() {
        let mut batch = Vec::new();
        let mut acks = Vec::new();
        let mut next = Some(first);
        while let Some(message) = next {
            match message {
                Message::Record(entry) => batch.push(entry),
                Message::Flush(ack) => acks.push(ack),
            }
            next = if batch.len() < MAX_BATCH {
                receiver.try_recv().ok()
            } else {
                None
            };
        }
        if !batch.is_empty()
            && let Err(err) = write_batch(db, &batch)
        {
            tracing::warn!(error = %err, lost = batch.len(), "token ledger batch not written");
            dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
            first_error.get_or_insert_with(|| err.to_string());
        }
        for ack in acks {
            // The flusher may have given up waiting; nothing to do then.
            let _ = ack.send(first_error.take());
        }
    }
}

fn write_batch(db: &Db, batch: &[Box<LedgerEntry>]) -> Result<()> {
    db.transaction(|tx| {
        let mut statement = tx.prepare_cached(
            "INSERT INTO token_ledger (
                 ts_ms, query_id, tier, locality, provider, model, purpose, prompt_tokens,
                 completion_tokens, cost_nanos, cloud_equivalent_nanos, latency_ms, ok, error,
                 usage_estimated, priced)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        for entry in batch {
            statement.execute(params![
                entry.timestamp_ms,
                entry.query_id,
                entry.tier.code(),
                entry.locality.code(),
                entry.provider,
                entry.model,
                entry.purpose.code(),
                signed(entry.prompt_tokens),
                signed(entry.completion_tokens),
                entry.cost_nanos,
                entry.cloud_equivalent_nanos,
                signed(entry.latency_ms),
                entry.ok,
                entry.error,
                entry.usage_estimated,
                entry.priced,
            ])?;
        }
        Ok(())
    })
}

/// SQLite integers are signed 64-bit; counts beyond that saturate.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn tier_from(code: i64) -> Result<ModelTier> {
    u8::try_from(code)
        .ok()
        .and_then(ModelTier::from_code)
        .ok_or_else(|| Error::integrity(format!("token ledger: unknown tier code {code}")))
}

fn locality_from(code: i64) -> Result<Locality> {
    u8::try_from(code)
        .ok()
        .and_then(Locality::from_code)
        .ok_or_else(|| Error::integrity(format!("token ledger: unknown locality code {code}")))
}

fn entry_from_row(row: &Row<'_>) -> Result<LedgerEntry> {
    let purpose: i64 = row.get("purpose")?;
    Ok(LedgerEntry {
        id: row.get("id")?,
        timestamp_ms: row.get("ts_ms")?,
        query_id: row.get("query_id")?,
        tier: tier_from(row.get("tier")?)?,
        locality: locality_from(row.get("locality")?)?,
        provider: row.get("provider")?,
        model: row.get("model")?,
        // A purpose added by a newer version reads as `Other` rather than failing the report.
        purpose: u8::try_from(purpose)
            .ok()
            .and_then(CallPurpose::from_code)
            .unwrap_or(CallPurpose::Other),
        prompt_tokens: unsigned(row.get("prompt_tokens")?),
        completion_tokens: unsigned(row.get("completion_tokens")?),
        cost_nanos: row.get("cost_nanos")?,
        cloud_equivalent_nanos: row.get("cloud_equivalent_nanos")?,
        latency_ms: unsigned(row.get("latency_ms")?),
        ok: row.get("ok")?,
        error: row.get("error")?,
        usage_estimated: row.get("usage_estimated")?,
        priced: row.get("priced")?,
    })
}

struct Sample {
    prompt: u64,
    completion: u64,
    cost: i64,
    equivalent: i64,
    latency: u64,
    ok: bool,
}

#[derive(Default)]
struct Accumulator {
    stats: UsageStats,
    latencies: Vec<u64>,
}

impl Accumulator {
    fn add(&mut self, sample: &Sample) {
        let stats = &mut self.stats;
        stats.calls += 1;
        stats.failures += u64::from(!sample.ok);
        stats.prompt_tokens = stats.prompt_tokens.saturating_add(sample.prompt);
        stats.completion_tokens = stats.completion_tokens.saturating_add(sample.completion);
        stats.cost_nanos = stats.cost_nanos.saturating_add(sample.cost);
        stats.cloud_equivalent_nanos = stats
            .cloud_equivalent_nanos
            .saturating_add(sample.equivalent);
        self.latencies.push(sample.latency);
    }

    fn finish(mut self) -> UsageStats {
        self.latencies.sort_unstable();
        let mut stats = self.stats;
        if !self.latencies.is_empty() {
            let sum: u128 = self.latencies.iter().map(|&value| u128::from(value)).sum();
            stats.mean_latency_ms = sum as f64 / self.latencies.len() as f64;
            stats.p50_latency_ms = percentile(&self.latencies, 50.0);
            stats.p95_latency_ms = percentile(&self.latencies, 95.0);
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::MigrationSet;
    use crate::base::TokenUsage;
    use crate::metrics::{MetricsConfig, ModelPrice};

    const SCHEMA: &[MigrationSet] = &[MigrationSet {
        module: "metrics",
        migrations: crate::metrics::MIGRATIONS,
    }];

    fn row_count(conn: &rusqlite::Connection) -> Result<u64> {
        Ok(unsigned(conn.query_row(
            "SELECT COUNT(*) FROM token_ledger",
            [],
            |row| row.get(0),
        )?))
    }

    fn ledger() -> TokenLedger {
        let db = Db::open_in_memory_with(SCHEMA).expect("db");
        TokenLedger::new(db, PriceTable::default()).expect("ledger")
    }

    fn call(ts: i64, locality: Locality, model: &str, prompt: u64, completion: u64) -> UsageRecord {
        let local = locality == Locality::Local;
        UsageRecord {
            timestamp_ms: ts,
            query_id: None,
            tier: if local {
                ModelTier::System1
            } else {
                ModelTier::System2
            },
            locality,
            provider: if local { "ollama" } else { "anthropic" }.into(),
            model: model.into(),
            purpose: if local {
                CallPurpose::Classify
            } else {
                CallPurpose::Synthesize
            },
            usage: TokenUsage::new(prompt, completion),
            usage_estimated: local,
            latency_ms: 100,
            ok: true,
            error: None,
        }
    }

    #[test]
    fn mixed_local_and_cloud_calls_are_accounted_to_the_nano_dollar() {
        let ledger = ledger();
        let mut local = call(1_000, Locality::Local, "qwen3", 1_200, 300);
        local.query_id = Some("q1".into());
        local.latency_ms = 40;
        ledger.record(local);
        let mut cloud = call(2_000, Locality::Cloud, "claude-fable-5-2", 10_000, 2_000);
        cloud.query_id = Some("q1".into());
        cloud.latency_ms = 900;
        ledger.record(cloud);
        let mut failed = UsageRecord {
            ok: false,
            error: Some("overloaded".into()),
            usage: TokenUsage::default(),
            latency_ms: 30_000,
            ..call(3_000, Locality::Cloud, "claude-fable-5-2", 0, 0)
        };
        failed.query_id = Some("q2".into());
        ledger.record(failed);
        ledger.record(call(4_000, Locality::Cloud, "mystery-1", 500, 500));
        ledger.flush().expect("flush");

        let summary = ledger.totals().expect("summary");
        // Local: free, valued at claude-opus-5-5 (4 / 20 USD per Mtok).
        let local_equivalent = 1_200 * 4_000 + 300 * 20_000;
        // Cloud: claude-fable-5-2 at 8 / 40.
        let cloud_cost = 10_000 * 8_000 + 2_000 * 40_000;
        assert_eq!(summary.local.calls, 1);
        assert_eq!(summary.local.cost_nanos, 0);
        assert_eq!(summary.local.cloud_equivalent_nanos, local_equivalent);
        assert_eq!(summary.cloud.calls, 3);
        assert_eq!(summary.cloud.failures, 1);
        assert_eq!(summary.cloud.cost_nanos, cloud_cost);
        assert_eq!(summary.cloud.cloud_equivalent_nanos, cloud_cost);
        assert_eq!(summary.cloud.prompt_tokens, 10_500);
        assert_eq!(summary.cloud.completion_tokens, 2_500);
        assert_eq!(summary.total.calls, 4);
        assert_eq!(summary.total.total_tokens(), 1_500 + 13_000);
        assert_eq!(summary.total.cost_nanos, cloud_cost);
        assert_eq!(summary.savings_nanos, local_equivalent);
        assert_eq!(summary.system1, summary.local);
        assert_eq!(summary.tier(ModelTier::System2), &summary.cloud);
        assert_eq!(summary.locality(Locality::Local).mean_latency_ms, 40.0);
        assert_eq!(summary.cloud.p50_latency_ms, 900);
        assert_eq!(summary.cloud.p95_latency_ms, 30_000);

        let for_query = ledger.for_query("q1").expect("for_query");
        assert_eq!(for_query.len(), 2);
        assert_eq!(for_query[0].locality, Locality::Local);
        assert!(for_query[0].usage_estimated && for_query[0].priced);
        assert_eq!(for_query[1].cost_nanos, cloud_cost);

        let recent = ledger.recent(2).expect("recent");
        assert_eq!(recent.len(), 2);
        // The unknown cloud model is recorded, unpriced, at zero cost.
        assert_eq!(recent[0].model, "mystery-1");
        assert!(!recent[0].priced);
        assert_eq!(
            (recent[0].cost_nanos, recent[0].cloud_equivalent_nanos),
            (0, 0)
        );
        // The failure is recorded with its error text.
        assert!(!recent[1].ok);
        assert_eq!(recent[1].error.as_deref(), Some("overloaded"));
        assert_eq!(recent[1].purpose, CallPurpose::Synthesize);

        let models = ledger.by_model(TimeRange::all()).expect("by_model");
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].model, "claude-fable-5-2");
        assert_eq!((models[0].calls, models[0].failures), (2, 1));
        assert_eq!(models[0].cost_nanos, cloud_cost);
        assert_eq!(models[1].model, "qwen3");
        assert!(models.iter().any(|m| m.model == "mystery-1" && !m.priced));
    }

    #[test]
    fn configured_baseline_and_prices_drive_the_recorded_costs() {
        let db = Db::open_in_memory_with(SCHEMA).expect("db");
        let prices = PriceTable::new(&MetricsConfig {
            pricing: vec![ModelPrice::new("openai_compatible", "gpt-*", 1.5, 6.0)],
        })
        .with_baseline("openai_compatible", "gpt-big");
        let ledger = TokenLedger::new(db, prices).expect("ledger");
        ledger.record(call(1, Locality::Local, "qwen3", 1_000, 100));
        ledger.record(UsageRecord {
            provider: "openai_compatible".into(),
            ..call(2, Locality::Cloud, "gpt-big", 2_000, 10)
        });
        ledger.flush().expect("flush");
        let summary = ledger.totals().expect("summary");
        assert_eq!(
            summary.local.cloud_equivalent_nanos,
            1_000 * 1_500 + 100 * 6_000
        );
        assert_eq!(summary.cloud.cost_nanos, 2_000 * 1_500 + 10 * 6_000);
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[7], 95.0), 7);
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50.0), 50);
        assert_eq!(percentile(&values, 95.0), 95);
        assert_eq!(percentile(&values, 100.0), 100);
        assert_eq!(percentile(&values, 0.0), 1);
        assert_eq!(percentile(&[10, 20, 30, 40], 50.0), 20);
        assert_eq!(percentile(&[10, 20, 30, 40], 95.0), 40);
        assert_eq!(percentile(&[10, 20, 30, 40], f64::NAN), 10);

        let ledger = ledger();
        for latency in 1..=20u64 {
            let mut record = call(1, Locality::Local, "m", 1, 1);
            record.latency_ms = latency * 10;
            ledger.record(record);
        }
        ledger.flush().expect("flush");
        let stats = ledger.totals().expect("summary").total;
        assert_eq!(stats.p50_latency_ms, 100);
        assert_eq!(stats.p95_latency_ms, 190);
        assert_eq!(stats.mean_latency_ms, 105.0);
    }

    #[test]
    fn time_ranges_and_buckets() {
        let ledger = ledger();
        let day = 1_800_000_000_000 - 1_800_000_000_000 % DAY_MS;
        for (offset, locality) in [
            (0, Locality::Local),
            (HOUR_MS - 1, Locality::Cloud),
            (HOUR_MS, Locality::Local),
            (DAY_MS + 5, Locality::Cloud),
        ] {
            ledger.record(call(day + offset, locality, "claude-opus-5-5", 1_000, 0));
        }
        ledger.flush().expect("flush");

        let hours = ledger
            .timeseries(Bucket::Hour, TimeRange::all())
            .expect("hours");
        assert_eq!(
            hours
                .iter()
                .map(|b| (b.start_ms - day, b.calls))
                .collect::<Vec<_>>(),
            vec![(0, 2), (HOUR_MS, 1), (DAY_MS, 1)]
        );
        assert_eq!((hours[0].local_calls, hours[0].cloud_calls), (1, 1));
        assert_eq!(hours[0].cost_nanos, 4_000_000);
        assert_eq!(hours[0].cloud_equivalent_nanos, 8_000_000);
        assert_eq!(hours[0].mean_latency_ms, 100.0);

        let days = ledger
            .timeseries(Bucket::Day, TimeRange::all())
            .expect("days");
        assert_eq!(days.iter().map(|b| b.calls).collect::<Vec<_>>(), vec![3, 1]);

        // Ranges are half-open: the upper bound is excluded, the lower included.
        let first_hour = TimeRange::between(day, day + HOUR_MS);
        assert_eq!(ledger.summary(first_hour).expect("summary").total.calls, 2);
        assert_eq!(
            ledger
                .summary(TimeRange::since(day + HOUR_MS))
                .expect("summary")
                .total
                .calls,
            2
        );
        assert_eq!(ledger.by_model(first_hour).expect("by_model").len(), 2);
        assert_eq!(
            ledger
                .cloud_spend_since_nanos(day + HOUR_MS)
                .expect("spend"),
            4_000_000
        );
        assert_eq!(Bucket::Day.start_of(day + 5), day);
        assert_eq!(Bucket::Hour.start_of(-1), -HOUR_MS);
    }

    #[test]
    fn cloud_spend_today_counts_only_todays_cloud_calls() {
        let ledger = ledger();
        let now = now_ms();
        ledger.record(call(
            now,
            Locality::Cloud,
            "claude-sonnet-5-5",
            1_000_000,
            0,
        ));
        ledger.record(call(now, Locality::Local, "qwen3", 1_000_000, 0));
        ledger.record(call(
            now - 2 * DAY_MS,
            Locality::Cloud,
            "claude-sonnet-5-5",
            5,
            5,
        ));
        ledger.flush().expect("flush");
        assert_eq!(
            ledger.cloud_spend_today_nanos().expect("nanos"),
            2_000_000_000
        );
        assert!((ledger.cloud_spend_today_usd().expect("usd") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn flush_makes_records_visible_and_drop_flushes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_with(dir.path().join("anchor.db"), SCHEMA).expect("db");
        let ledger = TokenLedger::new(db.clone(), PriceTable::default()).expect("ledger");
        ledger.flush().expect("flushing an empty queue is fine");
        for index in 0..50 {
            ledger.record(call(index, Locality::Local, "m", 1, 1));
        }
        ledger.flush().expect("flush");
        assert_eq!(db.read(row_count).expect("count"), 50);

        let clone = ledger.clone();
        for index in 0..25 {
            clone.record(call(index, Locality::Local, "m", 1, 1));
        }
        drop(clone);
        drop(ledger);
        // Dropping the last handle joined the writer, so everything is on disk.
        assert_eq!(db.read(row_count).expect("count"), 75);
    }

    #[test]
    fn concurrent_recording_loses_nothing() {
        let ledger = ledger();
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let ledger = ledger.clone();
                scope.spawn(move || {
                    for index in 0..250 {
                        ledger.record(call(
                            thread * 1_000 + index,
                            Locality::Cloud,
                            "claude-opus-5-5",
                            10,
                            1,
                        ));
                    }
                });
            }
        });
        ledger.flush().expect("flush");
        let total = ledger.totals().expect("summary").total;
        assert_eq!(ledger.dropped_records(), 0);
        assert_eq!(ledger.accepted_records(), 2_000);
        assert_eq!(total.calls, 2_000);
        assert_eq!(total.cost_nanos, 2_000 * (10 * 4_000 + 20_000));
    }

    #[test]
    fn overflow_is_counted_and_never_blocks_the_caller() {
        let db = Db::open_in_memory_with(SCHEMA).expect("db");
        let ledger =
            TokenLedger::with_capacity(db.clone(), PriceTable::default(), 4).expect("ledger");
        // Holding the write connection stalls the writer thread, so the queue must overflow.
        db.write(|_| {
            for index in 0..100 {
                ledger.record(call(index, Locality::Local, "m", 1, 1));
            }
            Ok(())
        })
        .expect("record while the writer is stalled");
        assert!(ledger.dropped_records() >= 1);
        assert_eq!(ledger.accepted_records() + ledger.dropped_records(), 100);
        ledger.flush().expect("flush");
        assert_eq!(
            db.read(row_count).expect("count"),
            ledger.accepted_records()
        );
    }

    #[test]
    fn write_failures_are_reported_by_flush_and_counted() {
        // No metrics schema: every insert fails, yet `record` neither panics nor errors.
        let db = Db::open_in_memory_with(&[]).expect("db");
        let ledger = TokenLedger::new(db, PriceTable::default()).expect("ledger");
        ledger.record(call(1, Locality::Local, "m", 1, 1));
        assert!(ledger.flush().is_err());
        assert_eq!(ledger.dropped_records(), 1);
        ledger.flush().expect("the error is reported once");
    }

    #[test]
    fn prune_removes_only_old_rows() {
        let ledger = ledger();
        for ts in [10, 20, 30] {
            ledger.record(call(ts, Locality::Local, "m", 1, 1));
        }
        // No explicit flush: prune flushes first.
        assert_eq!(ledger.prune(30).expect("prune"), 2);
        let left = ledger.recent(10).expect("recent");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].timestamp_ms, 30);
        assert_eq!(ledger.prune(30).expect("prune"), 0);
    }
}
