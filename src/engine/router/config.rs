//! Model tier configuration (`[models]`).

use serde::{Deserialize, Serialize};

use crate::ollama::DEFAULT_BASE_URL;

/// Which API a tier talks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// An Ollama server (local machine or LAN GPU box).
    Ollama,
    /// The Anthropic Messages API.
    Anthropic,
    /// Any endpoint speaking the OpenAI chat-completions protocol.
    #[serde(rename = "openai_compatible", alias = "openai")]
    OpenAiCompatible,
    /// The tier is switched off.
    #[default]
    Disabled,
}

/// One model tier.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TierConfig {
    /// API the tier talks to.
    pub provider: ProviderKind,
    /// Model identifier.
    pub model: String,
    /// Endpoint base URL; `None` uses the provider default.
    pub base_url: Option<String>,
    /// Name of the environment variable that holds the API key. The key itself is never written
    /// to the configuration file.
    pub api_key_env: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Default cap on generated tokens.
    pub max_tokens: u32,
    /// Default sampling temperature.
    pub temperature: f32,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Disabled,
            model: String::new(),
            base_url: None,
            api_key_env: None,
            timeout_secs: 120,
            max_tokens: 1024,
            temperature: 0.0,
        }
    }
}

impl TierConfig {
    /// `true` unless the tier is switched off.
    pub fn is_enabled(&self) -> bool {
        self.provider != ProviderKind::Disabled && !self.model.trim().is_empty()
    }
}

/// When to escalate from System 1 to System 2.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Master switch for metered cloud calls.
    pub allow_cloud: bool,
    /// Queries whose retrieval confidence falls below this value are escalated to System 2.
    pub escalation_confidence: f32,
    /// Daily spending cap for cloud calls in US dollars; `None` means unlimited.
    pub max_cloud_usd_per_day: Option<f64>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            allow_cloud: true,
            escalation_confidence: 0.6,
            max_cloud_usd_per_day: None,
        }
    }
}

/// The two model tiers and the routing policy between them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// System 1: fast local classifier/filter.
    pub system1: TierConfig,
    /// System 2: heavy reasoning engine. Disabled until configured.
    pub system2: TierConfig,
    /// Routing policy.
    pub routing: RoutingConfig,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            system1: TierConfig {
                provider: ProviderKind::Ollama,
                model: "llama3.2:3b".to_owned(),
                base_url: Some(DEFAULT_BASE_URL.to_owned()),
                timeout_secs: 60,
                max_tokens: 512,
                ..TierConfig::default()
            },
            system2: TierConfig::default(),
            routing: RoutingConfig::default(),
        }
    }
}
