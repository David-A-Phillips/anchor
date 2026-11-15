//! The tiered router: the only way the rest of the engine talks to a model.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::config::{ModelsConfig, RoutingConfig};
use super::filter::{
    FilterCandidate, MAX_FILTER_CANDIDATES, ScoredCandidate, filter_request, lexical_scores,
    parse_filter_scores,
};
use super::intent::{HeuristicClassifier, intent_request, parse_intent};
use super::policy::{RetrievalSignals, RouteDecision, System2Block, TierAvailability, route};
use super::provider::{HealthProbe, PROBE_TIMEOUT, TierDefaults, TierHandle, build_tier};
use crate::base::{
    Completion, CompletionRequest, FinishReason, LanguageModel, Locality, ModelError, ModelInfo,
    ModelTier, QueryIntent, Result, UsageRecord, UsageSink,
};

/// How long classification and filtering skip a System 1 model that just proved unreachable, so
/// that a stopped Ollama costs one connect timeout rather than one per query.
const SYSTEM1_COOLDOWN: Duration = Duration::from_secs(30);

/// Source of today's metered spend, implemented by the token ledger.
pub trait SpendTracker: Send + Sync {
    /// Estimated cost in US dollars of the cloud calls made today. Must be cheap: it is read
    /// before every System 2 call.
    fn cloud_spend_today_usd(&self) -> f64;
}

/// Bounded retry of transient failures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (0 disables retrying).
    pub max_retries: u32,
    /// Backoff before the first retry; doubles on each further retry.
    pub base_delay: Duration,
    /// Cap on a single wait. A server-requested `retry-after` beyond it ends the retries: the
    /// caller is better served by the fallback tier than by waiting.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (0-based) after `error`, or `None` when the error
    /// should not be retried.
    ///
    /// Retried: `Unavailable`, `RateLimited` and HTTP 5xx. Not retried: permanent errors, and
    /// `Timeout` — a request that used up its whole deadline has already cost the user the
    /// tier's timeout, and repeating it rarely ends differently.
    fn delay(&self, attempt: u32, error: &ModelError) -> Option<Duration> {
        if attempt >= self.max_retries
            || !error.is_transient()
            || matches!(error, ModelError::Timeout(_))
        {
            return None;
        }
        if let ModelError::RateLimited {
            retry_after: Some(wait),
        } = error
        {
            // Never earlier than the server asked; a little jitter spreads concurrent callers.
            return (*wait <= self.max_delay)
                .then(|| wait.mul_f64(1.0 + 0.1 * rand::random::<f64>()));
        }
        let backoff = self
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(self.max_delay);
        Some(backoff.mul_f64(0.5 + 0.5 * rand::random::<f64>()))
    }
}

/// A completion and how it was obtained.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutedCompletion {
    /// The model's answer.
    pub completion: Completion,
    /// The tier that actually produced it.
    pub tier: ModelTier,
    /// `true` when System 2 was requested but System 1 answered.
    pub degraded: bool,
}

/// Result of probing one tier.
#[derive(Clone, Debug, PartialEq)]
pub enum HealthStatus {
    /// No model is configured for the tier.
    NotConfigured,
    /// The endpoint answered the probe.
    Healthy,
    /// The tier has a model but no probe (a model injected without one).
    Unknown,
    /// The probe failed.
    Unhealthy(ModelError),
}

/// Health of one tier.
#[derive(Clone, Debug, PartialEq)]
pub struct TierHealth {
    /// The configured model, if any.
    pub info: Option<ModelInfo>,
    /// Probe outcome.
    pub status: HealthStatus,
}

/// Health of both tiers.
#[derive(Clone, Debug, PartialEq)]
pub struct RouterHealth {
    /// System 1.
    pub system1: TierHealth,
    /// System 2.
    pub system2: TierHealth,
}

struct Tier {
    model: Arc<dyn LanguageModel>,
    probe: Option<Arc<dyn HealthProbe>>,
    defaults: TierDefaults,
}

/// Routes model calls between System 1 (local, fast) and System 2 (cloud, heavy), applies
/// per-tier defaults, timeouts, retries and fallback, and reports every attempt to the
/// [`UsageSink`].
pub struct TieredRouter {
    system1: Option<Tier>,
    system2: Option<Tier>,
    sink: Arc<dyn UsageSink>,
    routing: RoutingConfig,
    spend: Option<Arc<dyn SpendTracker>>,
    retry: RetryPolicy,
    system1_down_until: Mutex<Option<Instant>>,
}

impl TieredRouter {
    /// Creates a router over already-built models (either may be absent). Tier defaults start as
    /// [`TierDefaults::default`]; see [`Self::with_defaults`].
    pub fn new(
        system1: Option<Arc<dyn LanguageModel>>,
        system2: Option<Arc<dyn LanguageModel>>,
        sink: Arc<dyn UsageSink>,
        routing: RoutingConfig,
    ) -> Self {
        let tier = |model| Tier {
            model,
            probe: None,
            defaults: TierDefaults::default(),
        };
        Self {
            system1: system1.map(tier),
            system2: system2.map(tier),
            sink,
            routing,
            spend: None,
            retry: RetryPolicy::default(),
            system1_down_until: Mutex::new(None),
        }
    }

    /// Builds both tiers from the `[models]` configuration. `env` resolves API-key variables
    /// (pass [`super::process_env`]). Fails when an enabled tier is misconfigured (bad URL,
    /// missing key).
    pub fn from_config(
        config: &ModelsConfig,
        sink: Arc<dyn UsageSink>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut router = Self::new(None, None, sink, config.routing.clone());
        if let Some(handle) = build_tier(ModelTier::System1, &config.system1, env)? {
            router = router.with_tier(ModelTier::System1, handle);
        }
        if let Some(handle) = build_tier(ModelTier::System2, &config.system2, env)? {
            router = router.with_tier(ModelTier::System2, handle);
        }
        Ok(router)
    }

    /// Installs a built tier (model, probe and defaults), replacing whatever was there.
    pub fn with_tier(mut self, tier: ModelTier, handle: TierHandle) -> Self {
        *self.slot(tier) = Some(Tier {
            model: handle.model,
            probe: Some(handle.probe),
            defaults: handle.defaults,
        });
        self
    }

    /// Sets the timeout and request defaults of a configured tier (no effect on an absent one).
    pub fn with_defaults(mut self, tier: ModelTier, defaults: TierDefaults) -> Self {
        if let Some(slot) = self.slot(tier) {
            slot.defaults = defaults;
        }
        self
    }

    /// Sets the health probe of a configured tier (no effect on an absent one).
    pub fn with_probe(mut self, tier: ModelTier, probe: Arc<dyn HealthProbe>) -> Self {
        if let Some(slot) = self.slot(tier) {
            slot.probe = Some(probe);
        }
        self
    }

    /// Enables the daily cloud budget guard (`RoutingConfig::max_cloud_usd_per_day`).
    pub fn with_spend_tracker(mut self, tracker: Arc<dyn SpendTracker>) -> Self {
        self.spend = Some(tracker);
        self
    }

    /// Replaces the retry policy.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    fn slot(&mut self, tier: ModelTier) -> &mut Option<Tier> {
        match tier {
            ModelTier::System1 => &mut self.system1,
            ModelTier::System2 => &mut self.system2,
        }
    }

    fn tier(&self, tier: ModelTier) -> Option<&Tier> {
        match tier {
            ModelTier::System1 => self.system1.as_ref(),
            ModelTier::System2 => self.system2.as_ref(),
        }
    }

    /// Description of the model configured for `tier`, if any.
    pub fn model_info(&self, tier: ModelTier) -> Option<&ModelInfo> {
        self.tier(tier).map(|tier| tier.model.info())
    }

    /// The routing configuration in force.
    pub fn routing(&self) -> &RoutingConfig {
        &self.routing
    }

    /// The tier's model if it may be called right now, otherwise why not. The cloud switch and
    /// the daily budget apply to any tier whose model is metered, whichever tier that is; local
    /// models are never blocked.
    fn usable(&self, tier: ModelTier) -> Result<&Tier, System2Block> {
        let tier = self.tier(tier).ok_or(System2Block::NotConfigured)?;
        if tier.model.info().locality == Locality::Cloud {
            if !self.routing.allow_cloud {
                return Err(System2Block::CloudDisallowed);
            }
            if let Some(cap) = self.routing.max_cloud_usd_per_day {
                let spent = self
                    .spend
                    .as_ref()
                    .map_or(0.0, |tracker| tracker.cloud_spend_today_usd());
                // `!(spent < cap)` also stops on a NaN from either side.
                if cap <= 0.0 || !(spent < cap) {
                    return Err(System2Block::BudgetExhausted);
                }
            }
        }
        Ok(tier)
    }

    fn block_error(tier: ModelTier, block: System2Block) -> ModelError {
        match block {
            System2Block::NotConfigured => {
                ModelError::NotConfigured(format!("no model is configured for {tier}"))
            }
            System2Block::CloudDisallowed => ModelError::NotConfigured(format!(
                "{tier} is a cloud model and cloud calls are disabled (models.routing.allow_cloud)"
            )),
            System2Block::BudgetExhausted => ModelError::BudgetExceeded(format!(
                "the daily cloud budget (models.routing.max_cloud_usd_per_day) is exhausted; {tier} is unavailable until tomorrow"
            )),
        }
    }

    /// One attempt: deadline, ledger record.
    async fn attempt(
        &self,
        tier: &Tier,
        request: CompletionRequest,
    ) -> Result<Completion, ModelError> {
        let info = tier.model.info();
        let purpose = request.purpose;
        let query_id = request.query_id.clone();
        let timeout = tier.defaults.timeout;
        let started = Instant::now();
        let result = match tokio::time::timeout(timeout, tier.model.complete(request)).await {
            Ok(result) => result,
            Err(_) => Err(ModelError::Timeout(timeout)),
        };
        let record = match &result {
            Ok(completion) => UsageRecord::success(info, purpose, query_id, completion),
            Err(error) => UsageRecord::failure(
                info,
                purpose,
                query_id,
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                error.to_string(),
            ),
        };
        self.sink.record(record);
        result
    }

    /// Applies the tier defaults and runs the request, retrying per `retry`.
    async fn call(
        &self,
        tier: &Tier,
        mut request: CompletionRequest,
        retry: &RetryPolicy,
    ) -> Result<Completion, ModelError> {
        request.max_tokens = request.max_tokens.or(tier.defaults.max_tokens);
        request.temperature = request.temperature.or(tier.defaults.temperature);
        let mut attempt = 0;
        loop {
            let error = match self.attempt(tier, request.clone()).await {
                Ok(completion) => return Ok(completion),
                Err(error) => error,
            };
            let Some(delay) = retry.delay(attempt, &error) else {
                return Err(error);
            };
            tracing::debug!(
                model = %tier.model.info().model,
                attempt,
                ?delay,
                %error,
                "transient model error, retrying"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// Runs `request` on `tier`.
    ///
    /// * Defaults: a request without `max_tokens` / `temperature` gets the tier's (providers
    ///   without sampling parameters ignore the temperature). Each attempt is bounded by the
    ///   tier timeout and recorded in the usage sink, success or failure, with its latency.
    /// * Retries: transient errors are retried per the [`RetryPolicy`], honouring `retry-after`.
    /// * Fallback: a System 2 request is served by System 1 with `degraded = true` when System 2
    ///   is not configured, disallowed (`allow_cloud`), over the daily budget, or fails after
    ///   retries. System 1 requests never escalate to System 2: that would spend money the caller
    ///   did not ask to spend.
    /// * Errors: `NotConfigured` when no model can serve the request (`BudgetExceeded` when the
    ///   budget is the only obstacle); otherwise the error of the requested tier.
    ///
    /// A provider refusal is not an error: it is a completion with
    /// [`FinishReason::Filtered`] and empty text, which callers must check.
    pub async fn complete(
        &self,
        tier: ModelTier,
        request: CompletionRequest,
    ) -> Result<RoutedCompletion, ModelError> {
        let routed = |completion, tier, degraded| RoutedCompletion {
            completion,
            tier,
            degraded,
        };
        let primary = match self.usable(tier) {
            Ok(model) => match self.call(model, request.clone(), &self.retry).await {
                Ok(completion) => return Ok(routed(completion, tier, false)),
                Err(error) => error,
            },
            Err(block) => Self::block_error(tier, block),
        };
        if tier != ModelTier::System2 {
            return Err(primary);
        }
        let Ok(local) = self.usable(ModelTier::System1) else {
            return Err(primary);
        };
        tracing::warn!(error = %primary, "System 2 unavailable, falling back to System 1");
        match self.call(local, request, &self.retry).await {
            Ok(completion) => Ok(routed(completion, ModelTier::System1, true)),
            // The requested tier's failure is the more useful one, unless it was only "absent".
            Err(fallback) if matches!(primary, ModelError::NotConfigured(_)) => Err(fallback),
            Err(_) => Err(primary),
        }
    }

    /// A single System 1 attempt for work that has a deterministic fallback: no retries, and a
    /// cool-down after an unreachable endpoint.
    async fn system1_once(&self, request: CompletionRequest) -> Option<Completion> {
        let tier = self.usable(ModelTier::System1).ok()?;
        if self
            .system1_down_until
            .lock()
            .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        let none = RetryPolicy {
            max_retries: 0,
            ..self.retry
        };
        match self.call(tier, request, &none).await {
            Ok(completion) => {
                *self.system1_down_until.lock() = None;
                (completion.finish_reason != FinishReason::Filtered).then_some(completion)
            }
            Err(error) => {
                if matches!(error, ModelError::Unavailable(_) | ModelError::Timeout(_)) {
                    *self.system1_down_until.lock() = Some(Instant::now() + SYSTEM1_COOLDOWN);
                }
                tracing::debug!(%error, "System 1 call failed, using the deterministic fallback");
                None
            }
        }
    }

    /// Intent parsing and query classification. Uses the System 1 model when one is usable and
    /// falls back to the [`HeuristicClassifier`] when there is none, the call fails, or its
    /// output is unusable. Never fails; `QueryIntent::classifier` tells which one answered.
    pub async fn classify(&self, query: &str, query_id: Option<&str>) -> QueryIntent {
        if let Some(completion) = self.system1_once(intent_request(query, query_id)).await {
            if let Some(intent) = parse_intent(&completion.text, query) {
                return intent;
            }
            tracing::debug!("System 1 returned an unusable classification");
        }
        HeuristicClassifier.classify(query)
    }

    /// Initial graph filtering: a relevance score in `[0, 1]` for every candidate, in input
    /// order. The first [`MAX_FILTER_CANDIDATES`] are judged by the System 1 model in one batched
    /// prompt; candidates beyond that, candidates the model skipped, and everything when the
    /// model is absent or fails, get the lexical-overlap score. Never fails.
    pub async fn filter(
        &self,
        query: &str,
        candidates: &[FilterCandidate],
        query_id: Option<&str>,
    ) -> Vec<ScoredCandidate> {
        let mut scores = lexical_scores(query, candidates);
        if candidates.is_empty() {
            return scores;
        }
        let judged = candidates.len().min(MAX_FILTER_CANDIDATES);
        if let Some(completion) = self
            .system1_once(filter_request(query, candidates, query_id))
            .await
            && let Some(model_scores) = parse_filter_scores(&completion.text, judged)
        {
            for (slot, score) in scores.iter_mut().zip(model_scores) {
                if let Some(score) = score {
                    slot.score = score;
                }
            }
        }
        scores
    }

    /// The routing decision for a query: the pure policy [`route`] applied to the current state
    /// of the tiers (configuration, cloud switch, today's spend).
    pub fn decide(&self, intent: &QueryIntent, signals: &RetrievalSignals) -> RouteDecision {
        let tiers = TierAvailability {
            system1: self.usable(ModelTier::System1).is_ok(),
            system2: self.usable(ModelTier::System2).map(|_| ()),
            escalation_confidence: self.routing.escalation_confidence,
        };
        route(intent, signals, &tiers)
    }

    async fn tier_health(&self, tier: ModelTier) -> TierHealth {
        let Some(tier) = self.tier(tier) else {
            return TierHealth {
                info: None,
                status: HealthStatus::NotConfigured,
            };
        };
        let status = match &tier.probe {
            None => HealthStatus::Unknown,
            // The outer deadline bounds probes that do not bound themselves.
            Some(probe) => match tokio::time::timeout(PROBE_TIMEOUT, probe.probe()).await {
                Ok(Ok(())) => HealthStatus::Healthy,
                Ok(Err(error)) => HealthStatus::Unhealthy(error),
                Err(_) => HealthStatus::Unhealthy(ModelError::Timeout(PROBE_TIMEOUT)),
            },
        };
        TierHealth {
            info: Some(tier.model.info().clone()),
            status,
        }
    }

    /// Probes both tiers concurrently. Probes are free (no tokens generated) and are not
    /// recorded in the usage sink.
    pub async fn health(&self) -> RouterHealth {
        let (system1, system2) = tokio::join!(
            self.tier_health(ModelTier::System1),
            self.tier_health(ModelTier::System2)
        );
        RouterHealth { system1, system2 }
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;
    use crate::base::{
        CallPurpose, ClassifierKind, Complexity, IntentKind, MemoryUsageSink, NodeKind, TokenUsage,
    };
    use crate::engine::router::policy::{EscalationTrigger, RouteReason};
    use crate::engine::router::scripted::{ScriptedModel, ScriptedReply};
    use crate::engine::router::testutil::serve;
    use crate::engine::router::{ProviderKind, TierConfig};

    struct Spent(f64);
    impl SpendTracker for Spent {
        fn cloud_spend_today_usd(&self) -> f64 {
            self.0
        }
    }

    fn scripted(tier: ModelTier, locality: Locality) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel::new(tier, locality))
    }

    fn router(
        system1: Option<&Arc<ScriptedModel>>,
        system2: Option<&Arc<ScriptedModel>>,
        routing: RoutingConfig,
    ) -> (TieredRouter, Arc<MemoryUsageSink>) {
        let sink = Arc::new(MemoryUsageSink::new());
        let as_dyn = |model: &Arc<ScriptedModel>| model.clone() as Arc<dyn LanguageModel>;
        let fast = RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_millis(2),
            max_delay: Duration::from_millis(200),
        };
        let router = TieredRouter::new(
            system1.map(as_dyn),
            system2.map(as_dyn),
            sink.clone(),
            routing,
        )
        .with_retry_policy(fast);
        (router, sink)
    }

    fn request() -> CompletionRequest {
        CompletionRequest::prompt(CallPurpose::Synthesize, "question").with_query_id("q-1")
    }

    fn unavailable() -> ModelError {
        ModelError::Unavailable("connection dropped".into())
    }

    #[tokio::test]
    async fn usage_is_ledgered_per_locality_across_real_providers() {
        let ollama = serve(
            Router::new()
                .route(
                    "/api/chat",
                    post(|| async {
                        Json(json!({
                            "model": "llama3.2:3b",
                            "message": {"role": "assistant", "content": "local answer"},
                            "done_reason": "stop",
                            "prompt_eval_count": 11,
                            "eval_count": 5
                        }))
                    }),
                )
                .route(
                    "/api/version",
                    get(|| async { Json(json!({"version": "0.9"})) }),
                ),
        )
        .await;
        let anthropic = serve(
            Router::new()
                .route(
                    "/v1/messages",
                    post(|Json(body): Json<serde_json::Value>| async move {
                        // Tier defaults reach the wire, minus what Anthropic forbids.
                        assert_eq!(body["max_tokens"], 2048);
                        assert!(body.get("temperature").is_none());
                        Json(json!({
                            "model": "claude-opus-5-5",
                            "stop_reason": "end_turn",
                            "content": [{"type": "text", "text": "cloud answer"}],
                            "usage": {"input_tokens": 100, "output_tokens": 20}
                        }))
                    }),
                )
                .route("/v1/models/{id}", get(|| async { Json(json!({})) })),
        )
        .await;
        let config = ModelsConfig {
            system1: TierConfig {
                provider: ProviderKind::Ollama,
                model: "llama3.2:3b".into(),
                base_url: Some(ollama),
                ..TierConfig::default()
            },
            system2: TierConfig {
                provider: ProviderKind::Anthropic,
                model: "claude-opus-5-5".into(),
                base_url: Some(anthropic),
                api_key_env: Some("TEST_KEY".into()),
                max_tokens: 2048,
                temperature: 0.3,
                ..TierConfig::default()
            },
            routing: RoutingConfig::default(),
        };
        let sink = Arc::new(MemoryUsageSink::new());
        let env = |name: &str| (name == "TEST_KEY").then(|| "sk-test".to_owned());
        let router = TieredRouter::from_config(&config, sink.clone(), &env).unwrap();

        for tier in [ModelTier::System1, ModelTier::System2, ModelTier::System1] {
            let routed = router.complete(tier, request()).await.unwrap();
            assert_eq!((routed.tier, routed.degraded), (tier, false));
        }
        assert_eq!(sink.total_for(Locality::Local), TokenUsage::new(22, 10));
        assert_eq!(sink.total_for(Locality::Cloud), TokenUsage::new(100, 20));
        let records = sink.records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].tier, ModelTier::System2);
        assert_eq!(records[1].provider, "anthropic");
        assert_eq!(records[1].purpose, CallPurpose::Synthesize);
        assert_eq!(records[1].query_id.as_deref(), Some("q-1"));
        assert!(
            records
                .iter()
                .all(|record| record.ok && !record.usage_estimated)
        );

        let health = router.health().await;
        assert_eq!(health.system1.status, HealthStatus::Healthy);
        assert_eq!(health.system2.status, HealthStatus::Healthy);
        assert_eq!(
            router.model_info(ModelTier::System2).unwrap().locality,
            Locality::Cloud
        );
    }

    #[tokio::test]
    async fn transient_errors_are_retried_and_every_attempt_is_recorded() {
        let cloud = scripted(ModelTier::System2, Locality::Cloud);
        cloud
            .push_error(ModelError::RateLimited {
                retry_after: Some(Duration::from_millis(60)),
            })
            .push_error(ModelError::Http {
                status: 529,
                body: "overloaded".into(),
            })
            .push(ScriptedReply::TextWithUsage(
                "done".into(),
                TokenUsage::new(40, 4),
            ));
        let (router, sink) = router(None, Some(&cloud), RoutingConfig::default());
        let started = Instant::now();
        let routed = router
            .complete(ModelTier::System2, request())
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "retry-after honoured"
        );
        assert_eq!(routed.completion.text, "done");
        assert!(!routed.degraded);
        let records = sink.records();
        assert_eq!(
            records.iter().map(|r| r.ok).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert!(
            records[0]
                .error
                .as_deref()
                .unwrap()
                .contains("rate limited")
        );
        assert_eq!(records[0].usage, TokenUsage::default());
        assert_eq!(sink.total_for(Locality::Cloud), TokenUsage::new(40, 4));

        // Retries are bounded, permanent errors and over-long retry-after are not retried.
        cloud.always(ScriptedReply::Error(unavailable()));
        assert_eq!(
            router.complete(ModelTier::System2, request()).await,
            Err(unavailable())
        );
        assert_eq!(cloud.calls(), 3 + 3);
        cloud.always(ScriptedReply::Error(ModelError::Auth("bad key".into())));
        assert!(matches!(
            router.complete(ModelTier::System2, request()).await,
            Err(ModelError::Auth(_))
        ));
        assert_eq!(cloud.calls(), 7);
        cloud.always(ScriptedReply::Error(ModelError::RateLimited {
            retry_after: Some(Duration::from_secs(3600)),
        }));
        assert!(
            router
                .complete(ModelTier::System2, request())
                .await
                .is_err()
        );
        assert_eq!(cloud.calls(), 8);
    }

    #[tokio::test]
    async fn system2_failure_falls_back_to_system1_degraded() {
        let local = scripted(ModelTier::System1, Locality::Local);
        let cloud = scripted(ModelTier::System2, Locality::Cloud);
        local.always(ScriptedReply::Text("local".into()));
        cloud.always(ScriptedReply::Error(unavailable()));
        let (router, sink) = router(Some(&local), Some(&cloud), RoutingConfig::default());
        let routed = router
            .complete(ModelTier::System2, request())
            .await
            .unwrap();
        assert_eq!((routed.tier, routed.degraded), (ModelTier::System1, true));
        assert_eq!(routed.completion.text, "local");
        assert_eq!((cloud.calls(), local.calls()), (3, 1));
        assert_eq!(sink.records().iter().filter(|r| !r.ok).count(), 3);

        // Both failing: the requested tier's error is reported.
        local.always(ScriptedReply::Error(ModelError::Other(
            "local broke".into(),
        )));
        assert_eq!(
            router.complete(ModelTier::System2, request()).await,
            Err(unavailable())
        );
        // System 1 requests never escalate.
        assert!(
            router
                .complete(ModelTier::System1, request())
                .await
                .is_err()
        );
        assert_eq!(cloud.calls(), 6);
    }

    #[tokio::test]
    async fn budget_and_cloud_switch_guard_system2() {
        let local = scripted(ModelTier::System1, Locality::Local);
        let cloud = scripted(ModelTier::System2, Locality::Cloud);
        local.always(ScriptedReply::Text("local".into()));
        cloud.always(ScriptedReply::Text("cloud".into()));
        let capped = RoutingConfig {
            max_cloud_usd_per_day: Some(5.0),
            ..RoutingConfig::default()
        };
        let complex = QueryIntent {
            complexity: Complexity::Complex,
            ..QueryIntent::general()
        };
        let signals = RetrievalSignals {
            confidence: 0.9,
            has_conflict: false,
            authoritative: true,
            context_tokens: 800,
        };

        // Under budget: System 2 serves.
        let (under, _) = router(Some(&local), Some(&cloud), capped.clone());
        let under = under.with_spend_tracker(Arc::new(Spent(4.99)));
        assert_eq!(
            under
                .complete(ModelTier::System2, request())
                .await
                .unwrap()
                .tier,
            ModelTier::System2
        );
        assert_eq!(
            under.decide(&complex, &signals).tier,
            Some(ModelTier::System2)
        );

        // Budget reached: degraded to System 1 without touching the cloud model.
        let (over, _) = router(Some(&local), Some(&cloud), capped.clone());
        let over = over.with_spend_tracker(Arc::new(Spent(5.0)));
        let routed = over.complete(ModelTier::System2, request()).await.unwrap();
        assert_eq!((routed.tier, routed.degraded), (ModelTier::System1, true));
        assert_eq!(cloud.calls(), 1);
        let decision = over.decide(&complex, &signals);
        assert_eq!(decision.tier, Some(ModelTier::System1));
        assert_eq!(
            decision.reason,
            RouteReason::LocalFallback(
                EscalationTrigger::ComplexIntent,
                System2Block::BudgetExhausted
            )
        );

        // Budget reached and no local model: a budget error, and no model in the decision.
        let (alone, sink) = router(None, Some(&cloud), capped);
        let alone = alone.with_spend_tracker(Arc::new(Spent(9.0)));
        assert!(matches!(
            alone.complete(ModelTier::System2, request()).await,
            Err(ModelError::BudgetExceeded(_))
        ));
        assert_eq!(alone.decide(&complex, &signals).tier, None);
        assert!(sink.is_empty());

        // Cloud switched off.
        let no_cloud = RoutingConfig {
            allow_cloud: false,
            ..RoutingConfig::default()
        };
        let (off, _) = router(Some(&local), Some(&cloud), no_cloud.clone());
        assert!(
            off.complete(ModelTier::System2, request())
                .await
                .unwrap()
                .degraded
        );
        assert_eq!(cloud.calls(), 1);
        let (off_alone, _) = router(None, Some(&cloud), no_cloud.clone());
        assert!(matches!(
            off_alone.complete(ModelTier::System2, request()).await,
            Err(ModelError::NotConfigured(_))
        ));

        // A local System 2 is metered by neither guard.
        let big_local = scripted(ModelTier::System2, Locality::Local);
        big_local.always(ScriptedReply::Text("big".into()));
        let (lan, _) = router(None, Some(&big_local), no_cloud);
        let routed = lan.complete(ModelTier::System2, request()).await.unwrap();
        assert_eq!((routed.tier, routed.degraded), (ModelTier::System2, false));
    }

    #[tokio::test]
    async fn timeouts_defaults_and_missing_models() {
        let local = scripted(ModelTier::System1, Locality::Local);
        local.push_delayed(Duration::from_secs(5), ScriptedReply::Text("late".into()));
        local.push_text("ok");
        let (router, sink) = router(Some(&local), None, RoutingConfig::default());
        let router = router.with_defaults(
            ModelTier::System1,
            TierDefaults {
                timeout: Duration::from_millis(40),
                max_tokens: Some(512),
                temperature: Some(0.2),
            },
        );
        let err = router
            .complete(ModelTier::System1, request())
            .await
            .unwrap_err();
        assert_eq!(err, ModelError::Timeout(Duration::from_millis(40)));
        // Timeouts are recorded with their latency and not retried.
        assert_eq!(local.calls(), 1);
        let record = &sink.records()[0];
        assert!(!record.ok && record.latency_ms >= 40);

        // Defaults fill what the request leaves open and never override it.
        router
            .complete(ModelTier::System1, request().with_max_tokens(7))
            .await
            .unwrap();
        let seen = local.requests();
        assert_eq!(
            (seen[0].max_tokens, seen[0].temperature),
            (Some(512), Some(0.2))
        );
        assert_eq!(
            (seen[1].max_tokens, seen[1].temperature),
            (Some(7), Some(0.2))
        );

        // System 2 requested, only System 1 exists: degraded service.
        local.push_text("fallback");
        let routed = router
            .complete(ModelTier::System2, request())
            .await
            .unwrap();
        assert!(routed.degraded);

        let (empty, sink) = super::tests::router(None, None, RoutingConfig::default());
        for tier in [ModelTier::System1, ModelTier::System2] {
            assert!(matches!(
                empty.complete(tier, request()).await,
                Err(ModelError::NotConfigured(_))
            ));
        }
        assert!(sink.is_empty());
        let health = empty.health().await;
        assert_eq!(health.system1.status, HealthStatus::NotConfigured);
        assert_eq!(health.system2.info, None);
    }

    #[tokio::test]
    async fn classify_uses_the_model_and_falls_back_to_the_heuristic() {
        let query = "Who owns API X?";
        let (bare, _) = router(None, None, RoutingConfig::default());
        let intent = bare.classify(query, None).await;
        assert_eq!(
            (intent.kind, intent.classifier),
            (IntentKind::Ownership, ClassifierKind::Heuristic)
        );

        let local = scripted(ModelTier::System1, Locality::Local);
        local
            .push_text("```json\n{\"intent\":\"dependency\",\"entities\":[{\"text\":\"X\",\"kind\":\"api_spec\"}],\"target_kinds\":[\"service\"],\"complexity\":\"moderate\",\"confidence\":0.7}\n```")
            .push_text("As an AI model I think this is about owners.")
            .push_error(unavailable());
        let (router, sink) = router(Some(&local), None, RoutingConfig::default());

        let intent = router.classify(query, Some("q-7")).await;
        assert_eq!(
            (intent.kind, intent.classifier),
            (IntentKind::Dependency, ClassifierKind::Model)
        );
        assert_eq!(intent.entities[0].kind_hint, Some(NodeKind::ApiSpec));
        let sent = &local.requests()[0];
        assert_eq!(sent.purpose, CallPurpose::Classify);
        assert_eq!(sent.query_id.as_deref(), Some("q-7"));

        // Garbage output: heuristic answer, but the call is still ledgered.
        let intent = router.classify(query, None).await;
        assert_eq!(
            (intent.kind, intent.classifier),
            (IntentKind::Ownership, ClassifierKind::Heuristic)
        );
        assert_eq!(sink.len(), 2);

        // Unreachable model: heuristic, one attempt only, then a cool-down without calls.
        let intent = router.classify(query, None).await;
        assert_eq!(intent.classifier, ClassifierKind::Heuristic);
        assert_eq!(local.calls(), 3);
        assert!(!sink.records()[2].ok);
        router.classify(query, None).await;
        assert_eq!(local.calls(), 3);
    }

    #[tokio::test]
    async fn filter_merges_model_scores_with_the_lexical_fallback() {
        let candidates: Vec<FilterCandidate> = [
            ("a", "payments-api"),
            ("b", "ledger"),
            ("c", "payments docs"),
        ]
        .into_iter()
        .map(|(id, name)| FilterCandidate {
            id: id.into(),
            kind: NodeKind::Service,
            name: name.into(),
            snippet: String::new(),
        })
        .collect();
        let local = scripted(ModelTier::System1, Locality::Local);
        local
            .push_text(r#"{"scores":[{"index":0,"score":0.9},{"index":1,"score":0.1}]}"#)
            .push_text("no json here");
        let (router, sink) = router(Some(&local), None, RoutingConfig::default());

        let scores = router.filter("payments", &candidates, Some("q-2")).await;
        let pairs: Vec<(&str, f32)> = scores.iter().map(|s| (s.id.as_str(), s.score)).collect();
        // The model scored a and b; c keeps its lexical score.
        assert_eq!(pairs, [("a", 0.9), ("b", 0.1), ("c", 1.0)]);
        assert_eq!(sink.records()[0].purpose, CallPurpose::Filter);

        let lexical = lexical_scores("payments", &candidates);
        assert_eq!(router.filter("payments", &candidates, None).await, lexical);
        assert!(router.filter("payments", &[], None).await.is_empty());
        assert_eq!(local.calls(), 2);
        let (bare, _) = super::tests::router(None, None, RoutingConfig::default());
        assert_eq!(bare.filter("payments", &candidates, None).await, lexical);
    }

    #[tokio::test]
    async fn health_reports_probe_results() {
        let local = scripted(ModelTier::System1, Locality::Local);
        let cloud = scripted(ModelTier::System2, Locality::Cloud);
        cloud.set_healthy(false);
        let (router, sink) = router(Some(&local), Some(&cloud), RoutingConfig::default());
        assert_eq!(router.health().await.system1.status, HealthStatus::Unknown);
        let router = router
            .with_probe(ModelTier::System1, local.clone())
            .with_probe(ModelTier::System2, cloud.clone());
        let health = router.health().await;
        assert_eq!(health.system1.status, HealthStatus::Healthy);
        assert!(matches!(
            health.system2.status,
            HealthStatus::Unhealthy(ModelError::Unavailable(_))
        ));
        assert_eq!(health.system2.info.unwrap().provider, "scripted");
        assert!(sink.is_empty() && local.calls() == 0);
    }
}
