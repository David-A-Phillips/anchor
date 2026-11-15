//! System 1 / System 2 model providers and tiered routing.
//!
//! * Providers ([`OllamaModel`], [`AnthropicModel`], [`OpenAiCompatModel`], and [`ScriptedModel`]
//!   for tests) implement [`LanguageModel`](crate::base::LanguageModel); [`build_model`] /
//!   [`build_tier`] construct them from a [`TierConfig`].
//! * System 1 functions: intent classification ([`HeuristicClassifier`], [`parse_intent`]) and
//!   candidate filtering ([`lexical_scores`], [`parse_filter_scores`]).
//! * [`TieredRouter`] ties them together: classification, filtering, the routing decision
//!   ([`route`]), and completions with defaults, timeouts, retries, fallback and usage records.

mod anthropic;
mod config;
mod filter;
mod intent;
mod ollama;
mod openai;
mod policy;
mod provider;
mod scripted;
mod tiered;

pub use anthropic::{
    ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL, ANTHROPIC_FALLBACK_BETA, ANTHROPIC_VERSION,
    AnthropicModel, DEFAULT_MAX_TOKENS,
};
pub use config::{ModelsConfig, ProviderKind, RoutingConfig, TierConfig};
pub use filter::{
    FilterCandidate, MAX_FILTER_CANDIDATES, ScoredCandidate, filter_request, lexical_scores,
    parse_filter_scores,
};
pub use intent::{
    HeuristicClassifier, default_target_kinds, intent_request, intent_schema, parse_intent,
};
pub use ollama::OllamaModel;
pub use openai::{OPENAI_BASE_URL, OpenAiCompatModel};
pub use policy::{
    EscalationTrigger, RetrievalSignals, RouteDecision, RouteReason, System2Block,
    TierAvailability, escalation_trigger, route,
};
pub use provider::{
    HealthProbe, TierDefaults, TierHandle, build_model, build_tier, endpoint_locality, process_env,
};
pub use scripted::{ScriptedModel, ScriptedReply};
pub use tiered::{
    HealthStatus, RetryPolicy, RoutedCompletion, RouterHealth, SpendTracker, TierHealth,
    TieredRouter,
};

#[cfg(test)]
pub(crate) mod testutil {
    /// Serves `app` on an ephemeral loopback port and returns its base URL.
    pub(crate) async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback port");
        let address = listener
            .local_addr()
            .expect("bound listener has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }
}
