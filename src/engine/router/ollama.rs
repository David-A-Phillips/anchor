//! System 1 provider: an Ollama server reached through [`OllamaApi::chat`].

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::provider::HealthProbe;
use crate::base::{
    Completion, CompletionRequest, FinishReason, LanguageModel, Locality, ModelError, ModelInfo,
    ModelTier, ResponseFormat, Result, Role, TokenUsage,
};
use crate::ollama::{ChatRequest, OllamaApi, OllamaError, OllamaMessage};

/// A chat model served by Ollama. Always [`Locality::Local`]: Ollama runs on hardware the user
/// controls, wherever it is on the network.
#[derive(Clone, Debug)]
pub struct OllamaModel {
    info: ModelInfo,
    api: OllamaApi,
    timeout: Duration,
    keep_alive: Option<String>,
    context_tokens: Option<u32>,
}

impl OllamaModel {
    /// Creates a provider for `model` on the Ollama server at `base_url`.
    pub fn new(tier: ModelTier, base_url: &str, model: &str) -> Result<Self> {
        Ok(Self {
            info: ModelInfo {
                tier,
                locality: Locality::Local,
                provider: "ollama".to_owned(),
                model: model.to_owned(),
            },
            api: OllamaApi::new(base_url)?,
            timeout: Duration::from_secs(60),
            // Keeps the model loaded between queries so classification stays fast.
            keep_alive: Some("10m".to_owned()),
            context_tokens: None,
        })
    }

    /// Sets the context window requested with every call (`num_ctx`; `None` or 0: server
    /// default). The same value on every call keeps the server from reloading the model.
    pub fn with_context_tokens(mut self, tokens: Option<u32>) -> Self {
        self.context_tokens = tokens.filter(|tokens| *tokens > 0);
        self
    }

    /// Sets the request deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets how long Ollama keeps the model loaded after a request (`None`: server default).
    pub fn with_keep_alive(mut self, keep_alive: Option<String>) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    fn map_error(&self, err: OllamaError) -> ModelError {
        match err {
            OllamaError::Unreachable { reason, .. } => ModelError::Unavailable(reason),
            OllamaError::Timeout { .. } => ModelError::Timeout(self.timeout),
            OllamaError::ModelNotFound(model) => ModelError::Http {
                status: 404,
                body: format!("model `{model}` is not available on the Ollama server"),
            },
            OllamaError::Status { status: 429, .. } => {
                ModelError::RateLimited { retry_after: None }
            }
            OllamaError::Status { status, body, .. } => ModelError::Http { status, body },
            OllamaError::Decode { reason, .. } => ModelError::InvalidResponse(reason),
            other => ModelError::Other(other.to_string()),
        }
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Builds the `/api/chat` body for a provider-independent request.
fn chat_request(
    model: &str,
    keep_alive: Option<&str>,
    context_tokens: Option<u32>,
    request: &CompletionRequest,
) -> ChatRequest {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        messages.push(OllamaMessage {
            role: "system".to_owned(),
            content: system.to_owned(),
        });
    }
    messages.extend(request.messages.iter().map(|message| OllamaMessage {
        role: role_name(message.role).to_owned(),
        content: message.content.clone(),
    }));
    let format = match &request.response_format {
        ResponseFormat::Text => None,
        ResponseFormat::Json => Some(json!("json")),
        ResponseFormat::JsonSchema(schema) => Some(schema.clone()),
    };
    let mut options = Map::new();
    if let Some(temperature) = request.temperature {
        options.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        options.insert("num_predict".to_owned(), json!(max_tokens));
    }
    if let Some(tokens) = context_tokens {
        options.insert("num_ctx".to_owned(), json!(tokens));
    }
    ChatRequest {
        model: model.to_owned(),
        messages,
        format,
        options: (!options.is_empty()).then_some(Value::Object(options)),
        keep_alive: keep_alive.map(str::to_owned),
        stream: false,
    }
}

#[async_trait]
impl LanguageModel for OllamaModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, ModelError> {
        let body = chat_request(
            &self.info.model,
            self.keep_alive.as_deref(),
            self.context_tokens,
            &request,
        );
        let started = Instant::now();
        let response = self
            .api
            .chat(&body, Some(self.timeout))
            .await
            .map_err(|err| self.map_error(err))?;
        let latency = started.elapsed();
        let text = response.message.content;
        // Ollama omits `prompt_eval_count` when the prompt was served from its cache, so a missing
        // half is estimated rather than counted as zero.
        let estimate = TokenUsage::estimate(request.prompt_chars(), text.len());
        let usage = TokenUsage::new(
            response.prompt_eval_count.unwrap_or(estimate.prompt_tokens),
            response.eval_count.unwrap_or(estimate.completion_tokens),
        );
        let finish_reason = match response.done_reason.as_deref() {
            None | Some("stop") => FinishReason::Stop,
            Some("length") => FinishReason::Length,
            Some(_) => FinishReason::Other,
        };
        Ok(Completion {
            text,
            usage,
            usage_estimated: response.prompt_eval_count.is_none() || response.eval_count.is_none(),
            latency,
            model: if response.model.is_empty() {
                self.info.model.clone()
            } else {
                response.model
            },
            finish_reason,
        })
    }
}

#[async_trait]
impl HealthProbe for OllamaModel {
    async fn probe(&self) -> Result<(), ModelError> {
        self.api
            .version()
            .await
            .map(|_| ())
            .map_err(|err| self.map_error(err))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use parking_lot::Mutex;

    use super::*;
    use crate::base::{CallPurpose, ChatMessage};
    use crate::engine::router::testutil::serve;

    type Seen = Arc<Mutex<Vec<Value>>>;

    async fn fake(reply: Value) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route(
                "/api/chat",
                post(move |State(seen): State<Seen>, Json(body): Json<Value>| {
                    let reply = reply.clone();
                    async move {
                        seen.lock().push(body);
                        Json(reply)
                    }
                }),
            )
            .route(
                "/api/version",
                get(|| async { Json(json!({"version": "0.9.0"})) }),
            )
            .with_state(seen.clone());
        (serve(app).await, seen)
    }

    #[tokio::test]
    async fn request_shape_usage_and_finish_reason() {
        let (url, seen) = fake(json!({
            "model": "llama3.2:3b",
            "message": {"role": "assistant", "content": "{\"a\":1}"},
            "done_reason": "length",
            "prompt_eval_count": 21,
            "eval_count": 8
        }))
        .await;
        let model = OllamaModel::new(ModelTier::System1, &url, "llama3.2:3b")
            .unwrap()
            .with_context_tokens(Some(4096));
        let schema = json!({"type": "object"});
        let mut request = CompletionRequest::prompt(CallPurpose::Classify, "hello")
            .with_system("be terse")
            .with_max_tokens(64)
            .with_temperature(0.0)
            .with_format(ResponseFormat::JsonSchema(schema.clone()));
        request
            .messages
            .insert(0, ChatMessage::assistant("earlier"));
        let completion = model.complete(request).await.unwrap();
        assert_eq!(completion.usage, TokenUsage::new(21, 8));
        assert!(!completion.usage_estimated);
        assert_eq!(completion.finish_reason, FinishReason::Length);
        assert_eq!(completion.text, "{\"a\":1}");

        let body = seen.lock()[0].clone();
        assert_eq!(body["model"], "llama3.2:3b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["keep_alive"], "10m");
        assert_eq!(body["format"], schema);
        assert_eq!(body["options"]["num_predict"], 64);
        assert_eq!(body["options"]["num_ctx"], 4096);
        assert_eq!(body["options"]["temperature"], 0.0);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "be terse"},
                {"role": "assistant", "content": "earlier"},
                {"role": "user", "content": "hello"}
            ])
        );
        model.probe().await.unwrap();
    }

    #[tokio::test]
    async fn json_mode_and_estimated_usage() {
        let (url, seen) = fake(json!({
            "message": {"role": "assistant", "content": "12345678"}
        }))
        .await;
        let model = OllamaModel::new(ModelTier::System1, &url, "m")
            .unwrap()
            .with_keep_alive(None);
        let request =
            CompletionRequest::prompt(CallPurpose::Other, "abcd").with_format(ResponseFormat::Json);
        let completion = model.complete(request).await.unwrap();
        assert!(completion.usage_estimated);
        assert_eq!(completion.usage, TokenUsage::new(1, 2));
        assert_eq!(completion.finish_reason, FinishReason::Stop);
        assert_eq!(completion.model, "m");
        let body = seen.lock()[0].clone();
        assert_eq!(body["format"], "json");
        assert!(body.get("options").is_none());
        assert!(body.get("keep_alive").is_none());
    }

    #[tokio::test]
    async fn errors_are_mapped() {
        let app = Router::new().route(
            "/api/chat",
            post(|| async { (axum::http::StatusCode::NOT_FOUND, "model not found") }),
        );
        let url = serve(app).await;
        let model = OllamaModel::new(ModelTier::System1, &url, "missing").unwrap();
        let err = model
            .complete(CompletionRequest::prompt(CallPurpose::Other, "x"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ModelError::Http { status: 404, .. }),
            "{err:?}"
        );
        assert!(!err.is_transient());
    }
}
