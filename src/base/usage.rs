//! The seam between model callers and the token ledger.
//!
//! The router reports every model call (successful or not) to a [`UsageSink`]. The metrics module
//! provides the SQLite-backed ledger; tests and tools can use [`MemoryUsageSink`].

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::llm::{CallPurpose, Completion, Locality, ModelInfo, ModelTier, TokenUsage};
use super::time::now_ms;

/// One model call, as reported to the ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// When the call finished, milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// Identifier of the user query the call belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Token counts (zero for failed calls that consumed nothing).
    pub usage: TokenUsage,
    /// `true` when the token counts were estimated rather than reported by the provider.
    #[serde(default)]
    pub usage_estimated: bool,
    /// Request latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Error text for failed calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UsageRecord {
    /// Builds the record of a successful call.
    pub fn success(
        info: &ModelInfo,
        purpose: CallPurpose,
        query_id: Option<String>,
        completion: &Completion,
    ) -> Self {
        Self {
            timestamp_ms: now_ms(),
            query_id,
            tier: info.tier,
            locality: info.locality,
            provider: info.provider.clone(),
            model: info.model.clone(),
            purpose,
            usage: completion.usage,
            usage_estimated: completion.usage_estimated,
            latency_ms: u64::try_from(completion.latency.as_millis()).unwrap_or(u64::MAX),
            ok: true,
            error: None,
        }
    }

    /// Builds the record of a failed call.
    pub fn failure(
        info: &ModelInfo,
        purpose: CallPurpose,
        query_id: Option<String>,
        latency_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_ms: now_ms(),
            query_id,
            tier: info.tier,
            locality: info.locality,
            provider: info.provider.clone(),
            model: info.model.clone(),
            purpose,
            usage: TokenUsage::default(),
            usage_estimated: false,
            latency_ms,
            ok: false,
            error: Some(error.into()),
        }
    }
}

/// Receives a record for every model call. Implementations must be cheap and must never panic or
/// block for long: recording usage must not be able to fail a user query.
pub trait UsageSink: Send + Sync {
    /// Records one model call.
    fn record(&self, record: UsageRecord);
}

/// Discards every record.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullUsageSink;

impl UsageSink for NullUsageSink {
    fn record(&self, _record: UsageRecord) {}
}

/// Keeps records in memory; for tests and one-shot tools.
#[derive(Debug, Default)]
pub struct MemoryUsageSink {
    records: Mutex<Vec<UsageRecord>>,
}

impl MemoryUsageSink {
    /// Creates an empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of everything recorded so far, oldest first.
    pub fn records(&self) -> Vec<UsageRecord> {
        self.records.lock().clone()
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.records.lock().len()
    }

    /// `true` when nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.records.lock().is_empty()
    }

    /// Sum of the token usage of all records for `locality`.
    pub fn total_for(&self, locality: Locality) -> TokenUsage {
        let records = self.records.lock();
        records
            .iter()
            .filter(|record| record.locality == locality)
            .fold(TokenUsage::default(), |acc, record| TokenUsage {
                prompt_tokens: acc.prompt_tokens + record.usage.prompt_tokens,
                completion_tokens: acc.completion_tokens + record.usage.completion_tokens,
            })
    }
}

impl UsageSink for MemoryUsageSink {
    fn record(&self, record: UsageRecord) {
        self.records.lock().push(record);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::base::llm::FinishReason;

    fn info(locality: Locality) -> ModelInfo {
        ModelInfo {
            tier: if locality == Locality::Local {
                ModelTier::System1
            } else {
                ModelTier::System2
            },
            locality,
            provider: "test".into(),
            model: "m".into(),
        }
    }

    #[test]
    fn memory_sink_accumulates_per_locality() {
        let sink = MemoryUsageSink::new();
        assert!(sink.is_empty());
        let completion = Completion {
            text: "ok".into(),
            usage: TokenUsage::new(10, 5),
            usage_estimated: false,
            latency: Duration::from_millis(42),
            model: "m".into(),
            finish_reason: FinishReason::Stop,
        };
        sink.record(UsageRecord::success(
            &info(Locality::Local),
            CallPurpose::Classify,
            Some("q1".into()),
            &completion,
        ));
        sink.record(UsageRecord::success(
            &info(Locality::Cloud),
            CallPurpose::Synthesize,
            Some("q1".into()),
            &completion,
        ));
        sink.record(UsageRecord::failure(
            &info(Locality::Cloud),
            CallPurpose::Synthesize,
            None,
            7,
            "boom",
        ));
        assert_eq!(sink.len(), 3);
        assert_eq!(sink.total_for(Locality::Local), TokenUsage::new(10, 5));
        assert_eq!(sink.total_for(Locality::Cloud), TokenUsage::new(10, 5));
        let records = sink.records();
        assert_eq!(records[0].latency_ms, 42);
        assert!(!records[2].ok);
        assert_eq!(records[2].error.as_deref(), Some("boom"));
    }
}
