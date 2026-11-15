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
}

/// Token ledger settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Price overrides and additions; entries here take precedence over the built-in price table.
    pub pricing: Vec<ModelPrice>,
}
