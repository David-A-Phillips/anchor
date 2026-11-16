//! Pieces shared by the model providers: construction from configuration, locality detection,
//! secret handling, HTTP error mapping and the health-probe contract.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::anthropic::AnthropicModel;
use super::config::{ProviderKind, TierConfig};
use super::ollama::OllamaModel;
use super::openai::OpenAiCompatModel;
use crate::base::{Error, LanguageModel, Locality, ModelError, ModelTier, Result};

/// Longest error body kept in a [`ModelError`].
const MAX_ERROR_BODY: usize = 500;

/// Deadline of a health probe. Short on purpose: an absent service may never refuse the connection.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A cheap liveness check of a model endpoint that never generates tokens.
#[async_trait]
pub trait HealthProbe: Send + Sync {
    /// Succeeds when the endpoint answers and accepts the configured credentials.
    async fn probe(&self) -> Result<(), ModelError>;
}

/// An API key that cannot leak through `Debug` or `Display`.
#[derive(Clone)]
pub(super) struct Secret(String);

impl Secret {
    pub(super) fn new(value: String) -> Self {
        Self(value)
    }

    /// The secret itself; only ever passed to an HTTP header.
    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Reads a variable from the process environment. The standard lookup to pass to [`build_model`].
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// A configured tier: the model, its health probe and the per-request defaults for the router.
#[derive(Clone)]
pub struct TierHandle {
    /// The provider.
    pub model: Arc<dyn LanguageModel>,
    /// Liveness probe of the same endpoint.
    pub probe: Arc<dyn HealthProbe>,
    /// Timeout and request defaults derived from the tier configuration.
    pub defaults: TierDefaults,
}

/// Per-tier request defaults applied by the router.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierDefaults {
    /// Deadline of one attempt.
    pub timeout: Duration,
    /// `max_tokens` for requests that set none; `None` leaves the choice to the provider.
    pub max_tokens: Option<u32>,
    /// Temperature for requests that set none. Ignored by providers without sampling parameters.
    pub temperature: Option<f32>,
}

impl Default for TierDefaults {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_tokens: None,
            temperature: None,
        }
    }
}

impl TierDefaults {
    /// Derives the defaults from a tier configuration.
    ///
    /// For Anthropic the generic `TierConfig` default of 1024 output tokens is treated as "not
    /// configured": current Claude models think before answering and thinking counts against
    /// `max_tokens`, so the provider's generous default applies unless the user chose a value.
    pub fn from_config(config: &TierConfig) -> Self {
        let unconfigured = config.max_tokens == TierConfig::default().max_tokens;
        let max_tokens = match config.provider {
            ProviderKind::Anthropic if unconfigured => None,
            _ if config.max_tokens == 0 => None,
            _ => Some(config.max_tokens),
        };
        Self {
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            max_tokens,
            temperature: Some(config.temperature),
        }
    }
}

fn handle<M>(model: M, config: &TierConfig) -> TierHandle
where
    M: LanguageModel + HealthProbe + 'static,
{
    let model = Arc::new(model);
    TierHandle {
        model: model.clone(),
        probe: model,
        defaults: TierDefaults::from_config(config),
    }
}

/// Builds the provider, probe and defaults of one tier; `None` when the tier is disabled.
///
/// `env` resolves the variable named by [`TierConfig::api_key_env`]; pass [`process_env`] in
/// production and a closure in tests. A missing key for a provider that requires one is a
/// configuration error.
pub fn build_tier(
    tier: ModelTier,
    config: &TierConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<TierHandle>> {
    if !config.is_enabled() {
        return Ok(None);
    }
    let timeout = Duration::from_secs(config.timeout_secs.max(1));
    let built = match config.provider {
        ProviderKind::Disabled => return Ok(None),
        ProviderKind::Ollama => {
            let base = config
                .base_url
                .as_deref()
                .unwrap_or(crate::ollama::DEFAULT_BASE_URL);
            handle(
                OllamaModel::new(tier, base, &config.model)?.with_timeout(timeout),
                config,
            )
        }
        ProviderKind::Anthropic => {
            let var = config.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY");
            let key = env(var).ok_or_else(|| {
                Error::config(format!(
                    "{tier} uses Anthropic but the environment variable {var} is not set"
                ))
            })?;
            handle(
                AnthropicModel::new(tier, &config.model, key, config.base_url.as_deref())?
                    .with_timeout(timeout),
                config,
            )
        }
        ProviderKind::OpenAiCompatible => {
            let key = match config.api_key_env.as_deref() {
                Some(var) => Some(env(var).ok_or_else(|| {
                    Error::config(format!(
                        "{tier} names the API key variable {var} but it is not set"
                    ))
                })?),
                None => None,
            };
            handle(
                OpenAiCompatModel::new(tier, &config.model, key, config.base_url.as_deref())?
                    .with_timeout(timeout),
                config,
            )
        }
    };
    Ok(Some(built))
}

/// Builds the provider of one tier; `None` when the tier is disabled. See [`build_tier`].
pub fn build_model(
    tier: ModelTier,
    config: &TierConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Arc<dyn LanguageModel>>> {
    Ok(build_tier(tier, config, env)?.map(|handle| handle.model))
}

/// Locality of an HTTP endpoint: `Local` when its host is `localhost`, a loopback address or a
/// private / link-local / unique-local address, `Cloud` otherwise (including unparsable URLs, so
/// that an unknown endpoint is never mistaken for a free one).
pub fn endpoint_locality(base_url: &str) -> Locality {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return Locality::Cloud;
    };
    let Some(host) = url.host_str() else {
        return Locality::Cloud;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return Locality::Local;
    }
    let local = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
        Err(_) => false,
    };
    if local {
        Locality::Local
    } else {
        Locality::Cloud
    }
}

/// Validates and normalises a base URL (http/https, no trailing slash).
pub(super) fn normalize_base(base_url: &str) -> Result<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed)
        .map_err(|err| Error::config(format!("invalid model base URL `{base_url}`: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::config(format!(
            "invalid model base URL `{base_url}`: expected http(s)://host"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Truncates `text` to at most `max` bytes on a character boundary.
pub(super) fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// Maps a transport failure. Request deadlines become `Timeout`, everything else `Unavailable`.
pub(super) fn transport_error(err: &reqwest::Error, timeout: Duration) -> ModelError {
    if err.is_timeout() && !err.is_connect() {
        ModelError::Timeout(timeout)
    } else {
        // `without_url` keeps hostnames out of ledger rows; the cause chain stays informative.
        ModelError::Unavailable(describe(err))
    }
}

fn describe(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    truncate(text, MAX_ERROR_BODY)
}

/// Parses a `retry-after` header given in (possibly fractional) seconds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds.min(86_400.0)))
}

/// Turns a non-success HTTP response into a [`ModelError`]:
/// 401/403 → `Auth`, 429 → `RateLimited` (with `retry-after`), everything else → `Http`, whose
/// transience follows the status (5xx, 529 included, is transient; 4xx is permanent).
pub(super) async fn status_error(response: reqwest::Response) -> ModelError {
    let status = response.status().as_u16();
    let retry_after = retry_after(response.headers());
    let body = truncate(response.text().await.unwrap_or_default(), MAX_ERROR_BODY);
    match status {
        401 | 403 => ModelError::Auth(format!("HTTP {status}: {body}")),
        429 => ModelError::RateLimited { retry_after },
        _ => ModelError::Http { status, body },
    }
}

/// Runs a credentialed GET as a health probe.
pub(super) async fn probe_get(request: reqwest::RequestBuilder) -> Result<(), ModelError> {
    let response = request
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|err| transport_error(&err, PROBE_TIMEOUT))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(status_error(response).await)
    }
}

crate::base::opaque_debug!(TierHandle);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locality_follows_the_host() {
        for local in [
            "http://localhost:8080/v1",
            "http://127.0.0.1:1234",
            "http://10.1.2.3/v1",
            "http://192.168.1.20:8000/v1",
            "http://172.16.0.9",
            "http://[::1]:8080/v1",
            "http://[fd00::1]/v1",
        ] {
            assert_eq!(endpoint_locality(local), Locality::Local, "{local}");
        }
        for cloud in [
            "https://api.openai.com/v1",
            "https://8.8.8.8/v1",
            "http://172.32.0.1",
            "not a url",
        ] {
            assert_eq!(endpoint_locality(cloud), Locality::Cloud, "{cloud}");
        }
    }

    #[test]
    fn build_model_honours_provider_and_environment() {
        let env = |name: &str| (name == "MY_KEY").then(|| "sk-secret".to_owned());
        let none = |_: &str| None;

        let disabled = TierConfig::default();
        assert!(
            build_model(ModelTier::System2, &disabled, &env)
                .unwrap()
                .is_none()
        );

        let ollama = TierConfig {
            provider: ProviderKind::Ollama,
            model: "llama3.2:3b".into(),
            ..TierConfig::default()
        };
        let model = build_model(ModelTier::System1, &ollama, &none)
            .unwrap()
            .unwrap();
        assert_eq!(model.info().locality, Locality::Local);
        assert_eq!(model.info().provider, "ollama");
        assert_eq!(model.info().tier, ModelTier::System1);

        let anthropic = TierConfig {
            provider: ProviderKind::Anthropic,
            model: "claude-opus-5-5".into(),
            api_key_env: Some("MY_KEY".into()),
            ..TierConfig::default()
        };
        let model = build_model(ModelTier::System2, &anthropic, &env)
            .unwrap()
            .unwrap();
        assert_eq!(model.info().locality, Locality::Cloud);
        assert_eq!(model.info().provider, "anthropic");
        // A missing key is a configuration error whose text names the variable, not a secret.
        let err = build_model(ModelTier::System2, &anthropic, &none)
            .err()
            .unwrap();
        assert!(err.to_string().contains("MY_KEY"));

        let lan = TierConfig {
            provider: ProviderKind::OpenAiCompatible,
            model: "qwen".into(),
            base_url: Some("http://192.168.0.5:8000/v1".into()),
            ..TierConfig::default()
        };
        let model = build_model(ModelTier::System2, &lan, &none)
            .unwrap()
            .unwrap();
        assert_eq!(model.info().locality, Locality::Local);
        let hosted = TierConfig {
            base_url: Some("https://api.example.com/v1".into()),
            api_key_env: Some("MY_KEY".into()),
            ..lan
        };
        let model = build_model(ModelTier::System2, &hosted, &env)
            .unwrap()
            .unwrap();
        assert_eq!(model.info().locality, Locality::Cloud);
        assert_eq!(model.info().provider, "openai_compatible");
    }

    #[test]
    fn defaults_and_secrets() {
        let anthropic = TierConfig {
            provider: ProviderKind::Anthropic,
            model: "claude-opus-5-5".into(),
            ..TierConfig::default()
        };
        assert_eq!(TierDefaults::from_config(&anthropic).max_tokens, None);
        let tuned = TierConfig {
            max_tokens: 4000,
            timeout_secs: 7,
            ..anthropic
        };
        let defaults = TierDefaults::from_config(&tuned);
        assert_eq!(defaults.max_tokens, Some(4000));
        assert_eq!(defaults.timeout, Duration::from_secs(7));
        assert_eq!(
            format!("{:?}", Secret::new("sk-secret".into())),
            "Secret(***)"
        );
        assert_eq!(truncate("héllo".into(), 2), "h");
    }
}
