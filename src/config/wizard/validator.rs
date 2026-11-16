//! Endpoint validation: every enabled tier must answer a real, tiny completion before the wizard
//! may write the configuration.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::base::{CallPurpose, CompletionRequest, LanguageModel, ModelError, ModelTier};
use crate::engine::router::{ProviderKind, TierConfig, build_model};

/// Environment lookup the wizard and its validators share.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Completions slower than this are reported as slow (but still pass).
const SLOW: Duration = Duration::from_secs(20);

/// Outcome of validating one tier.
#[derive(Clone, Debug, PartialEq)]
pub struct TierValidation {
    /// The tier.
    pub tier: ModelTier,
    /// The configuration that was validated.
    pub config: TierConfig,
    /// `true` when the endpoint is reachable, authenticated and the model answered.
    pub ok: bool,
    /// Round-trip time of the test completion, when one completed.
    pub latency: Option<Duration>,
    /// One line for the user: what worked or what to fix.
    pub message: String,
}

impl TierValidation {
    /// A passing validation.
    pub fn passed(
        tier: ModelTier,
        config: &TierConfig,
        latency: Duration,
        message: String,
    ) -> Self {
        Self {
            tier,
            config: config.clone(),
            ok: true,
            latency: Some(latency),
            message,
        }
    }

    /// A failing validation.
    pub fn failed(tier: ModelTier, config: &TierConfig, message: impl Into<String>) -> Self {
        Self {
            tier,
            config: config.clone(),
            ok: false,
            latency: None,
            message: message.into(),
        }
    }
}

/// Checks that a tier works.
#[async_trait]
pub trait EndpointValidator: Send + Sync {
    /// Validates `config` as `tier`. Never errors: a problem is a failed [`TierValidation`].
    async fn validate(&self, tier: ModelTier, config: &TierConfig, env: &EnvFn) -> TierValidation;
}

/// Builds the model of a tier; `None` for a disabled tier.
pub type ModelFactory = dyn Fn(ModelTier, &TierConfig, &EnvFn) -> crate::Result<Option<Arc<dyn LanguageModel>>>
    + Send
    + Sync;

/// Validates by building the model with a factory and sending one tiny completion. The default
/// factory is the model router's `build_model`, so what passes here is exactly what the engine
/// will run.
pub struct ModelValidator {
    factory: Arc<ModelFactory>,
}

impl Default for ModelValidator {
    fn default() -> Self {
        Self::router()
    }
}

impl ModelValidator {
    /// The real validator, built on `engine::router`.
    pub fn router() -> Self {
        Self::with_factory(|tier, config, env| {
            let env = env.clone();
            build_model(tier, config, &move |name| env(name))
        })
    }

    /// A validator over any model factory (tests plug in scripted models).
    pub fn with_factory(
        factory: impl Fn(
            ModelTier,
            &TierConfig,
            &EnvFn,
        ) -> crate::Result<Option<Arc<dyn LanguageModel>>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
        }
    }
}

fn explain(error: &ModelError, config: &TierConfig) -> String {
    match error {
        ModelError::Unavailable(reason) => format!("endpoint unreachable: {reason}"),
        ModelError::Auth(reason) => format!(
            "authentication failed ({reason}); check the key stored in {}",
            config
                .api_key_env
                .as_deref()
                .unwrap_or("the API key variable")
        ),
        ModelError::Http { status: 404, .. } => format!(
            "model `{}` was not found at the endpoint (pull it or pick another)",
            config.model
        ),
        other => other.to_string(),
    }
}

#[async_trait]
impl EndpointValidator for ModelValidator {
    async fn validate(&self, tier: ModelTier, config: &TierConfig, env: &EnvFn) -> TierValidation {
        let model = match (self.factory)(tier, config, env) {
            Ok(Some(model)) => model,
            Ok(None) => return TierValidation::failed(tier, config, "the tier is disabled"),
            Err(err) => return TierValidation::failed(tier, config, err.to_string()),
        };
        let mut request =
            CompletionRequest::prompt(CallPurpose::Validate, "Reply with the single word: ok");
        request.max_tokens = Some(16);
        request.temperature = Some(0.0);
        let deadline = Duration::from_secs(config.timeout_secs.max(1)) + Duration::from_secs(2);
        match tokio::time::timeout(deadline, model.complete(request)).await {
            Err(_) => TierValidation::failed(
                tier,
                config,
                format!("no answer within {} s", deadline.as_secs()),
            ),
            Ok(Err(err)) => TierValidation::failed(tier, config, explain(&err, config)),
            Ok(Ok(completion)) => {
                let slow = if completion.latency > SLOW {
                    " (slow: expect sluggish queries)"
                } else {
                    ""
                };
                let message = format!(
                    "{} `{}` answered in {} ms{slow}",
                    provider_name(config.provider),
                    config.model,
                    completion.latency.as_millis()
                );
                TierValidation::passed(tier, config, completion.latency, message)
            }
        }
    }
}

/// Human name of a provider.
pub fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Ollama => "Ollama",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::OpenAiCompatible => "OpenAI-compatible",
        ProviderKind::Disabled => "disabled",
    }
}

crate::base::opaque_debug!(ModelValidator);
