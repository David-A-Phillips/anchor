//! Metrics configuration (`[metrics]`).

use serde::{Deserialize, Serialize};

/// Price of one model, in US dollars per million tokens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Provider name as reported by the model (`"anthropic"`, `"openai_compatible"`, `"ollama"`).
    pub provider: String,
    /// Model identifier; a trailing `*` matches any suffix.
    pub model: String,
    /// Price of one million prompt (input) tokens.
    pub input_usd_per_mtok: f64,
    /// Price of one million completion (output) tokens.
    pub output_usd_per_mtok: f64,
    /// Long-prompt tier: calls whose prompt has **more** than this many tokens are charged at the
    /// `long_*` prices (for the whole call, input and output). `None`: a single flat price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_prompt_threshold_tokens: Option<u64>,
    /// Input price above the threshold; defaults to `input_usd_per_mtok` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_input_usd_per_mtok: Option<f64>,
    /// Output price above the threshold; defaults to `output_usd_per_mtok` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_output_usd_per_mtok: Option<f64>,
}

impl ModelPrice {
    /// A flat price (no long-prompt tier).
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        input_usd_per_mtok: f64,
        output_usd_per_mtok: f64,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            input_usd_per_mtok,
            output_usd_per_mtok,
            long_prompt_threshold_tokens: None,
            long_input_usd_per_mtok: None,
            long_output_usd_per_mtok: None,
        }
    }

    /// Adds a long-prompt tier: prompts with more than `threshold_tokens` tokens are charged at
    /// the given prices.
    pub fn with_long_prompt_tier(
        mut self,
        threshold_tokens: u64,
        input_usd_per_mtok: f64,
        output_usd_per_mtok: f64,
    ) -> Self {
        self.long_prompt_threshold_tokens = Some(threshold_tokens);
        self.long_input_usd_per_mtok = Some(input_usd_per_mtok);
        self.long_output_usd_per_mtok = Some(output_usd_per_mtok);
        self
    }
}

/// Token ledger settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Price overrides and additions; entries here take precedence over the built-in price table.
    pub pricing: Vec<ModelPrice>,
}
