//! The language-model contract shared by every provider and every caller.
//!
//! Anchor routes work between two tiers:
//!
//! * **System 1** — a fast local model (Ollama) used for intent parsing, query classification and
//!   initial graph filtering;
//! * **System 2** — a heavy reasoning model (normally a cloud API) used for complex reasoning,
//!   context expansion and synthesis.
//!
//! Providers implement [`LanguageModel`]; callers never talk to a provider directly but go through
//! the tiered router, which also records every call in the token ledger.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::macros::string_enum;

string_enum! {
    /// The reasoning tier a model serves.
    pub enum ModelTier {
        /// Fast local classifier/filter.
        System1 = 1 => "system1",
        /// Heavy reasoning and synthesis engine.
        System2 = 2 => "system2",
    }
}

string_enum! {
    /// Where a model runs. This is the "Local vs. Cloud" axis of the token ledger.
    pub enum Locality {
        /// Runs on hardware the user controls (localhost or LAN Ollama); no per-token charge.
        Local = 1 => "local",
        /// Runs behind a metered third-party API.
        Cloud = 2 => "cloud",
    }
}

string_enum! {
    /// Why a model was called. Recorded in the token ledger.
    pub enum CallPurpose {
        /// Intent parsing and query classification.
        Classify = 1 => "classify",
        /// Initial graph filtering: judging candidate relevance.
        Filter = 2 => "filter",
        /// Answer synthesis from assembled context.
        Synthesize = 3 => "synthesize",
        /// Context expansion: deciding what else to retrieve.
        Expand = 4 => "expand",
        /// Endpoint validation during setup or health checks.
        Validate = 5 => "validate",
        /// Background backlog processing.
        Backlog = 6 => "backlog",
        /// Anything else.
        Other = 7 => "other",
    }
}

string_enum! {
    /// Author of a chat message.
    pub enum Role {
        /// Instructions to the model.
        System = 1 => "system",
        /// The user or the calling program.
        User = 2 => "user",
        /// A previous model reply.
        Assistant = 3 => "assistant",
    }
}

/// One chat message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who wrote the message.
    pub role: Role,
    /// Message text.
    pub content: String,
}

impl ChatMessage {
    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// An assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Shape of the output the caller expects.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free text.
    #[default]
    Text,
    /// Any syntactically valid JSON value.
    Json,
    /// JSON conforming to the given JSON Schema. Providers that cannot enforce a schema fall back
    /// to plain JSON mode and embed the schema in the prompt.
    JsonSchema(serde_json::Value),
}

/// A provider-independent completion request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// System prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Conversation so far; the last message is normally a user message.
    pub messages: Vec<ChatMessage>,
    /// Upper bound on generated tokens; `None` uses the tier default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature; `None` uses the tier default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Expected output shape.
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Why the call is made (ledger attribution).
    pub purpose: CallPurpose,
    /// Identifier of the user query this call belongs to (ledger correlation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_id: Option<String>,
}

impl CompletionRequest {
    /// A single-turn request with one user message.
    pub fn prompt(purpose: CallPurpose, prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![ChatMessage::user(prompt)],
            max_tokens: None,
            temperature: None,
            response_format: ResponseFormat::Text,
            purpose,
            query_id: None,
        }
    }

    /// Sets the system prompt (builder style).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the output token limit (builder style).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the sampling temperature (builder style).
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Sets the expected output shape (builder style).
    pub fn with_format(mut self, format: ResponseFormat) -> Self {
        self.response_format = format;
        self
    }

    /// Sets the correlation id (builder style).
    pub fn with_query_id(mut self, query_id: impl Into<String>) -> Self {
        self.query_id = Some(query_id.into());
        self
    }

    /// Total characters of prompt text; used to estimate tokens when a provider reports none.
    pub fn prompt_chars(&self) -> usize {
        self.system.as_deref().map_or(0, str::len)
            + self.messages.iter().map(|m| m.content.len()).sum::<usize>()
    }
}

/// Token counts of one call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens in the prompt (input).
    pub prompt_tokens: u64,
    /// Tokens generated (output).
    pub completion_tokens: u64,
}

impl TokenUsage {
    /// Creates a usage record.
    pub const fn new(prompt_tokens: u64, completion_tokens: u64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
        }
    }

    /// Prompt plus completion tokens.
    pub const fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    /// Rough token estimate for providers that do not report usage: one token per four bytes of
    /// text, rounded up.
    pub fn estimate(prompt_chars: usize, completion_chars: usize) -> Self {
        Self {
            prompt_tokens: prompt_chars.div_ceil(4) as u64,
            completion_tokens: completion_chars.div_ceil(4) as u64,
        }
    }
}

string_enum! {
    /// Why generation stopped.
    pub enum FinishReason {
        /// The model finished its answer.
        Stop = 1 => "stop",
        /// The output token limit was reached.
        Length = 2 => "length",
        /// The provider withheld or filtered the output.
        Filtered = 3 => "filtered",
        /// Unknown or provider-specific reason.
        Other = 4 => "other",
    }
}

/// The result of a completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    /// Generated text.
    pub text: String,
    /// Token counts.
    pub usage: TokenUsage,
    /// `true` when `usage` was estimated because the provider reported none.
    #[serde(default)]
    pub usage_estimated: bool,
    /// Wall-clock duration of the request.
    pub latency: Duration,
    /// Model identifier as reported by the provider.
    pub model: String,
    /// Why generation stopped.
    pub finish_reason: FinishReason,
}

/// Static description of a configured model.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Tier the model serves.
    pub tier: ModelTier,
    /// Where it runs.
    pub locality: Locality,
    /// Provider name, e.g. `"ollama"`, `"anthropic"`, `"openai_compatible"`.
    pub provider: String,
    /// Model identifier, e.g. `"llama3.2:3b"`.
    pub model: String,
}

/// Failure of a model call.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// The endpoint could not be reached (connection refused, DNS failure, connect timeout).
    #[error("model endpoint unavailable: {0}")]
    Unavailable(String),
    /// The request did not complete within its deadline.
    #[error("model request timed out after {0:?}")]
    Timeout(Duration),
    /// The provider rejected the credentials.
    #[error("model authentication failed: {0}")]
    Auth(String),
    /// The provider throttled the request.
    #[error("model rate limited (retry after {retry_after:?})")]
    RateLimited {
        /// Server-suggested delay, when provided.
        retry_after: Option<Duration>,
    },
    /// The provider returned an error status.
    #[error("model endpoint returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// The response could not be interpreted.
    #[error("invalid model response: {0}")]
    InvalidResponse(String),
    /// The configured spending or token budget would be exceeded.
    #[error("model budget exceeded: {0}")]
    BudgetExceeded(String),
    /// No model is configured for the requested tier.
    #[error("no model configured: {0}")]
    NotConfigured(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl ModelError {
    /// `true` when retrying the same request later may succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            ModelError::Unavailable(_)
            | ModelError::Timeout(_)
            | ModelError::RateLimited { .. } => true,
            ModelError::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// A chat-completion capable model.
#[async_trait]
pub trait LanguageModel: Send + Sync {
    /// Static description of the model.
    fn info(&self) -> &ModelInfo;

    /// Runs one completion. Implementations must fill [`Completion::latency`] and
    /// [`Completion::usage`] (estimating when the provider reports none) and must not retry
    /// internally; retry policy belongs to the router.
    async fn complete(&self, request: CompletionRequest) -> Result<Completion, ModelError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_estimate_rounds_up() {
        assert_eq!(TokenUsage::estimate(9, 0), TokenUsage::new(3, 0));
        assert_eq!(TokenUsage::estimate(0, 1).total(), 1);
    }

    #[test]
    fn transient_errors_are_classified() {
        assert!(ModelError::Unavailable("refused".into()).is_transient());
        assert!(
            ModelError::Http {
                status: 503,
                body: String::new()
            }
            .is_transient()
        );
        assert!(
            !ModelError::Http {
                status: 400,
                body: String::new()
            }
            .is_transient()
        );
        assert!(!ModelError::Auth("bad key".into()).is_transient());
    }

    #[test]
    fn request_builder_counts_prompt_chars() {
        let request = CompletionRequest::prompt(CallPurpose::Classify, "hello")
            .with_system("be brief")
            .with_max_tokens(16)
            .with_format(ResponseFormat::Json);
        assert_eq!(request.prompt_chars(), 13);
        assert_eq!(request.max_tokens, Some(16));
        assert_eq!(request.response_format, ResponseFormat::Json);
    }
}
