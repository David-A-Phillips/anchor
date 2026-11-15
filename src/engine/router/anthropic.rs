//! System 2 provider: the Anthropic Messages API over raw HTTP.
//!
//! Request rules that current Claude models enforce (each violation is an HTTP 400), and that this
//! provider therefore hard-codes:
//!
//! * **No sampling parameters and no `thinking` block.** `temperature`, `top_p`, `top_k` and
//!   thinking budgets are rejected, so [`CompletionRequest::temperature`] and
//!   `TierConfig::temperature` are ignored for this provider.
//! * **`max_tokens` is required.** Requests that set none get [`DEFAULT_MAX_TOKENS`].
//! * **JSON through structured outputs** (`output_config.format`), never assistant prefill and
//!   never forced tool choice.
//! * **Refusals** arrive as HTTP 200 with `stop_reason: "refusal"`. The provider opts in to
//!   server-side fallbacks so a declined request is retried on Anthropic's recommended substitute
//!   inside the same call; a refusal that survives is returned as a completion with
//!   [`FinishReason::Filtered`] and empty text.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::provider::{
    HealthProbe, Secret, normalize_base, probe_get, status_error, transport_error,
};
use crate::base::net::http_client;
use crate::base::{
    Completion, CompletionRequest, FinishReason, LanguageModel, Locality, ModelError, ModelInfo,
    ModelTier, ResponseFormat, Result, Role, TokenUsage,
};

/// Default Anthropic endpoint.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Model used when the configuration names none.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Value of the required `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Beta header that enables the `fallbacks` request parameter.
pub const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// `max_tokens` for requests that set none. Generous because thinking counts against it; still
/// well inside what a non-streaming request can return before HTTP timeouts become a concern.
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// A Claude model behind the Anthropic Messages API. Always [`Locality::Cloud`].
#[derive(Clone, Debug)]
pub struct AnthropicModel {
    info: ModelInfo,
    base_url: String,
    api_key: Secret,
    http: reqwest::Client,
    timeout: Duration,
}

impl AnthropicModel {
    /// Creates a provider. An empty `model` selects [`ANTHROPIC_DEFAULT_MODEL`]; `base_url`
    /// defaults to [`ANTHROPIC_BASE_URL`]. The key is only ever sent in the `x-api-key` header.
    pub fn new(
        tier: ModelTier,
        model: &str,
        api_key: String,
        base_url: Option<&str>,
    ) -> Result<Self> {
        let model = match model.trim() {
            "" => ANTHROPIC_DEFAULT_MODEL,
            name => name,
        };
        Ok(Self {
            info: ModelInfo {
                tier,
                locality: Locality::Cloud,
                provider: "anthropic".to_owned(),
                model: model.to_owned(),
            },
            base_url: normalize_base(base_url.unwrap_or(ANTHROPIC_BASE_URL))?,
            api_key: Secret::new(api_key),
            http: http_client(None)?,
            timeout: Duration::from_secs(120),
        })
    }

    /// Sets the request deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Haiku has no server-side fallback and rejects fallback lists, so the opt-in is skipped.
    fn supports_fallbacks(&self) -> bool {
        !self.info.model.contains("haiku")
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header("x-api-key", self.api_key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION)
    }
}

/// Builds the Messages API body. Never contains sampling parameters, `thinking` or tools.
fn messages_body(model: &str, fallbacks: bool, request: &CompletionRequest) -> Value {
    // The API takes the system prompt as a top-level field; system-role messages are folded in.
    let mut system: Vec<&str> = request.system.as_deref().into_iter().collect();
    let mut messages = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let role = match message.role {
            Role::System => {
                system.push(&message.content);
                continue;
            }
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        messages.push(json!({"role": role, "content": message.content}));
    }
    let mut system = system.join("\n\n");
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model));
    body.insert(
        "max_tokens".to_owned(),
        json!(request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(1)),
    );
    match &request.response_format {
        ResponseFormat::Text => {}
        ResponseFormat::Json => {
            // Structured outputs need a schema; schema-less JSON is requested in the prompt.
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(
                "Respond with a single valid JSON value and nothing else: no prose, no code fences.",
            );
        }
        ResponseFormat::JsonSchema(schema) => {
            body.insert(
                "output_config".to_owned(),
                json!({"format": {"type": "json_schema", "schema": schema}}),
            );
        }
    }
    if !system.is_empty() {
        body.insert("system".to_owned(), json!(system));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if fallbacks {
        body.insert("fallbacks".to_owned(), json!("default"));
    }
    Value::Object(body)
}

#[derive(Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[async_trait]
impl LanguageModel for AnthropicModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, ModelError> {
        let fallbacks = self.supports_fallbacks();
        let body = messages_body(&self.info.model, fallbacks, &request);
        let mut http = self
            .authorized(self.http.post(format!("{}/v1/messages", self.base_url)))
            .timeout(self.timeout)
            .json(&body);
        if fallbacks {
            http = http.header("anthropic-beta", ANTHROPIC_FALLBACK_BETA);
        }
        let started = Instant::now();
        let response = http
            .send()
            .await
            .map_err(|err| transport_error(&err, self.timeout))?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let parsed: MessagesResponse = response.json().await.map_err(|err| {
            if err.is_timeout() {
                ModelError::Timeout(self.timeout)
            } else {
                ModelError::InvalidResponse(format!("undecodable Messages API response: {err}"))
            }
        })?;
        let latency = started.elapsed();

        // The stop reason decides whether `content` means anything: a refusal may carry partial
        // or schema-violating output, which is discarded.
        let finish_reason = match parsed.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence") => FinishReason::Stop,
            Some("max_tokens" | "model_context_window_exceeded") => FinishReason::Length,
            Some("refusal") => FinishReason::Filtered,
            _ => FinishReason::Other,
        };
        let text = if finish_reason == FinishReason::Filtered {
            String::new()
        } else {
            parsed
                .content
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect()
        };
        let (usage, usage_estimated) = match parsed.usage {
            Some(usage) => (
                TokenUsage::new(
                    usage.input_tokens
                        + usage.cache_creation_input_tokens.unwrap_or(0)
                        + usage.cache_read_input_tokens.unwrap_or(0),
                    usage.output_tokens,
                ),
                false,
            ),
            None => (
                TokenUsage::estimate(request.prompt_chars(), text.len()),
                true,
            ),
        };
        Ok(Completion {
            text,
            usage,
            usage_estimated,
            latency,
            // With fallbacks the serving model can differ from the requested one.
            model: parsed.model.unwrap_or_else(|| self.info.model.clone()),
            finish_reason,
        })
    }
}

#[async_trait]
impl HealthProbe for AnthropicModel {
    /// `GET /v1/models/{model}`: free, and verifies both the key and the model id.
    async fn probe(&self) -> Result<(), ModelError> {
        let url = format!("{}/v1/models/{}", self.base_url, self.info.model);
        probe_get(self.authorized(self.http.get(url))).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use parking_lot::Mutex;

    use super::*;
    use crate::base::{CallPurpose, ChatMessage};
    use crate::engine::router::testutil::serve;

    type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    async fn fake(
        status: StatusCode,
        headers: &[(&'static str, &'static str)],
        reply: Value,
    ) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let headers = headers.to_vec();
        let app = Router::new()
            .route(
                "/v1/messages",
                post(
                    move |State(seen): State<Seen>,
                          request_headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let reply = reply.clone();
                        let headers = headers.clone();
                        async move {
                            seen.lock().push((request_headers, body));
                            let mut out = HeaderMap::new();
                            for (name, value) in headers {
                                out.insert(name, value.parse().unwrap());
                            }
                            (status, out, Json(reply)).into_response()
                        }
                    },
                ),
            )
            .route(
                "/v1/models/{id}",
                get(|headers: HeaderMap| async move {
                    if headers.get("x-api-key").is_some_and(|key| key == "sk-test") {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            )
            .with_state(seen.clone());
        (serve(app).await, seen)
    }

    fn model(url: &str) -> AnthropicModel {
        AnthropicModel::new(ModelTier::System2, "", "sk-test".into(), Some(url)).unwrap()
    }

    fn ok_reply() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "Team "},
                {"type": "text", "text": "Payments"}
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 7,
                "cache_read_input_tokens": 3
            }
        })
    }

    #[tokio::test]
    async fn request_shape_headers_and_usage() {
        let (url, seen) = fake(StatusCode::OK, &[], ok_reply()).await;
        let model = model(&url);
        assert_eq!(model.info().model, "claude-opus-5-5");
        let schema = json!({"type": "object", "properties": {}, "additionalProperties": false});
        let mut request = CompletionRequest::prompt(CallPurpose::Synthesize, "Who owns it?")
            .with_system("Answer from the context.")
            .with_temperature(0.7)
            .with_format(ResponseFormat::JsonSchema(schema.clone()));
        request.messages.insert(
            0,
            ChatMessage {
                role: Role::System,
                content: "Extra rule.".into(),
            },
        );
        let completion = model.complete(request).await.unwrap();
        assert_eq!(completion.text, "Team Payments");
        assert_eq!(completion.usage, TokenUsage::new(110, 20));
        assert!(!completion.usage_estimated);
        assert_eq!(completion.finish_reason, FinishReason::Stop);

        let (headers, body) = seen.lock()[0].clone();
        assert_eq!(headers["x-api-key"], "sk-test");
        assert_eq!(headers["anthropic-version"], "2026-10-21");
        assert_eq!(headers["anthropic-beta"], "server-side-fallback-2026-07-01");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 16_000);
        assert_eq!(body["system"], "Answer from the context.\n\nExtra rule.");
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "Who owns it?"}])
        );
        assert_eq!(
            body["output_config"],
            json!({"format": {"type": "json_schema", "schema": schema}})
        );
        assert_eq!(body["fallbacks"], "default");
        for forbidden in [
            "temperature",
            "top_p",
            "top_k",
            "thinking",
            "tools",
            "tool_choice",
            "stream",
        ] {
            assert!(
                body.get(forbidden).is_none(),
                "{forbidden} must not be sent"
            );
        }
        model.probe().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_max_tokens_plain_json_and_haiku() {
        let (url, seen) = fake(
            StatusCode::OK,
            &[],
            json!({"stop_reason": "max_tokens", "content": []}),
        )
        .await;
        let model = AnthropicModel::new(
            ModelTier::System2,
            "claude-haiku-5-5",
            "k".into(),
            Some(&url),
        )
        .unwrap();
        let request = CompletionRequest::prompt(CallPurpose::Other, "abcdefgh")
            .with_max_tokens(300)
            .with_format(ResponseFormat::Json);
        let completion = model.complete(request).await.unwrap();
        assert_eq!(completion.finish_reason, FinishReason::Length);
        assert!(completion.usage_estimated);
        assert_eq!(completion.usage.prompt_tokens, 2);
        let (headers, body) = seen.lock()[0].clone();
        assert_eq!(body["max_tokens"], 300);
        assert!(body["system"].as_str().unwrap().contains("JSON"));
        assert!(body.get("output_config").is_none());
        assert!(body.get("fallbacks").is_none());
        assert!(headers.get("anthropic-beta").is_none());
        // The probe authenticates with the configured key, which this fake rejects.
        assert!(matches!(model.probe().await, Err(ModelError::Auth(_))));
    }

    #[tokio::test]
    async fn refusal_discards_content() {
        let (url, _) = fake(
            StatusCode::OK,
            &[],
            json!({
                "model": "claude-opus-4-8",
                "stop_reason": "refusal",
                "content": [{"type": "text", "text": "partial out"}],
                "usage": {"input_tokens": 9, "output_tokens": 2}
            }),
        )
        .await;
        let completion = model(&url)
            .complete(CompletionRequest::prompt(CallPurpose::Synthesize, "x"))
            .await
            .unwrap();
        assert_eq!(completion.finish_reason, FinishReason::Filtered);
        assert_eq!(completion.text, "");
        assert_eq!(completion.usage, TokenUsage::new(9, 2));
        assert_eq!(completion.model, "claude-opus-4-8");
    }

    #[tokio::test]
    async fn error_statuses_are_mapped() {
        let request = || CompletionRequest::prompt(CallPurpose::Other, "x");
        let error = json!({"type": "error", "error": {"type": "x", "message": "nope"}});
        for (status, transient) in [
            (400, false),
            (404, false),
            (413, false),
            (500, true),
            (529, true),
        ] {
            let (url, _) = fake(StatusCode::from_u16(status).unwrap(), &[], error.clone()).await;
            let err = model(&url).complete(request()).await.unwrap_err();
            assert!(
                matches!(&err, ModelError::Http { status: s, body } if *s == status && body.contains("nope")),
                "{err:?}"
            );
            assert_eq!(err.is_transient(), transient, "{status}");
        }
        for status in [401, 403] {
            let (url, _) = fake(StatusCode::from_u16(status).unwrap(), &[], error.clone()).await;
            let err = model(&url).complete(request()).await.unwrap_err();
            assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
            assert!(!err.to_string().contains("sk-test"));
        }
        let (url, _) = fake(
            StatusCode::TOO_MANY_REQUESTS,
            &[("retry-after", "7")],
            error.clone(),
        )
        .await;
        let err = model(&url).complete(request()).await.unwrap_err();
        assert_eq!(
            err,
            ModelError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        let (url, _) = fake(StatusCode::OK, &[], json!("not an object")).await;
        let err = model(&url).complete(request()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn slow_endpoint_times_out() {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                StatusCode::OK
            }),
        );
        let url = serve(app).await;
        let model = model(&url).with_timeout(Duration::from_millis(80));
        let err = model
            .complete(CompletionRequest::prompt(CallPurpose::Other, "x"))
            .await
            .unwrap_err();
        assert_eq!(err, ModelError::Timeout(Duration::from_millis(80)));
        assert!(!format!("{model:?}").contains("sk-test"));
    }
}
