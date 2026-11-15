//! Provider for endpoints speaking the OpenAI chat-completions protocol (OpenAI itself, vLLM,
//! llama.cpp, LM Studio, gateways).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::provider::{
    HealthProbe, Secret, endpoint_locality, normalize_base, probe_get, status_error,
    transport_error,
};
use crate::base::net::http_client;
use crate::base::{
    Completion, CompletionRequest, FinishReason, LanguageModel, ModelError, ModelInfo, ModelTier,
    ResponseFormat, Result, Role, TokenUsage,
};

/// Endpoint used when the configuration names none.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// A model behind `POST {base}/chat/completions`. Its locality follows the endpoint host (see
/// [`endpoint_locality`]).
#[derive(Clone, Debug)]
pub struct OpenAiCompatModel {
    info: ModelInfo,
    base_url: String,
    api_key: Option<Secret>,
    http: reqwest::Client,
    timeout: Duration,
}

impl OpenAiCompatModel {
    /// Creates a provider. `base_url` includes the version prefix (`.../v1`) and defaults to
    /// [`OPENAI_BASE_URL`]. Without a key no `Authorization` header is sent (local servers).
    pub fn new(
        tier: ModelTier,
        model: &str,
        api_key: Option<String>,
        base_url: Option<&str>,
    ) -> Result<Self> {
        let base_url = normalize_base(base_url.unwrap_or(OPENAI_BASE_URL))?;
        Ok(Self {
            info: ModelInfo {
                tier,
                locality: endpoint_locality(&base_url),
                provider: "openai_compatible".to_owned(),
                model: model.to_owned(),
            },
            base_url,
            api_key: api_key.map(Secret::new),
            http: http_client(None)?,
            timeout: Duration::from_secs(120),
        })
    }

    /// Sets the request deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key.expose()),
            None => request,
        }
    }
}

fn chat_body(model: &str, request: &CompletionRequest) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        messages.push(json!({"role": role, "content": message.content}));
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), json!(false));
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    match &request.response_format {
        ResponseFormat::Text => {}
        ResponseFormat::Json => {
            body.insert("response_format".to_owned(), json!({"type": "json_object"}));
        }
        ResponseFormat::JsonSchema(schema) => {
            body.insert(
                "response_format".to_owned(),
                json!({
                    "type": "json_schema",
                    "json_schema": {"name": "response", "schema": schema, "strict": true}
                }),
            );
        }
    }
    Value::Object(body)
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[async_trait]
impl LanguageModel for OpenAiCompatModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, ModelError> {
        let body = chat_body(&self.info.model, &request);
        let http = self
            .authorized(
                self.http
                    .post(format!("{}/chat/completions", self.base_url)),
            )
            .timeout(self.timeout)
            .json(&body);
        let started = Instant::now();
        let response = http
            .send()
            .await
            .map_err(|err| transport_error(&err, self.timeout))?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let parsed: ChatResponse = response.json().await.map_err(|err| {
            if err.is_timeout() {
                ModelError::Timeout(self.timeout)
            } else {
                ModelError::InvalidResponse(format!("undecodable chat completion: {err}"))
            }
        })?;
        let latency = started.elapsed();
        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            ModelError::InvalidResponse("chat completion contains no choices".to_owned())
        })?;
        let finish_reason = match choice.finish_reason.as_deref() {
            None | Some("stop") => FinishReason::Stop,
            Some("length") => FinishReason::Length,
            Some("content_filter") => FinishReason::Filtered,
            Some(_) => FinishReason::Other,
        };
        let text = choice
            .message
            .and_then(|message| message.content)
            .unwrap_or_default();
        let estimate = TokenUsage::estimate(request.prompt_chars(), text.len());
        let reported = parsed
            .usage
            .and_then(|usage| Some((usage.prompt_tokens?, usage.completion_tokens?)));
        let (usage, usage_estimated) = match reported {
            Some((prompt, completion)) => (TokenUsage::new(prompt, completion), false),
            None => (estimate, true),
        };
        Ok(Completion {
            text,
            usage,
            usage_estimated,
            latency,
            model: parsed.model.unwrap_or_else(|| self.info.model.clone()),
            finish_reason,
        })
    }
}

#[async_trait]
impl HealthProbe for OpenAiCompatModel {
    /// `GET {base}/models`: supported by every implementation of the protocol and free.
    async fn probe(&self) -> Result<(), ModelError> {
        probe_get(self.authorized(self.http.get(format!("{}/models", self.base_url)))).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use parking_lot::Mutex;

    use super::*;
    use crate::base::{CallPurpose, Locality};
    use crate::engine::router::testutil::serve;

    type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    async fn fake(status: StatusCode, reply: Value) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    move |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| {
                        let reply = reply.clone();
                        async move {
                            seen.lock().push((headers, body));
                            (status, [("retry-after", "0.5")], Json(reply))
                        }
                    },
                ),
            )
            .route("/v1/models", get(|| async { Json(json!({"data": []})) }))
            .with_state(seen.clone());
        (format!("{}/v1", serve(app).await), seen)
    }

    #[tokio::test]
    async fn request_shape_usage_and_finish_reason() {
        let (url, seen) = fake(
            StatusCode::OK,
            json!({
                "model": "gpt-x",
                "choices": [{"message": {"role": "assistant", "content": "{}"}, "finish_reason": "content_filter"}],
                "usage": {"prompt_tokens": 31, "completion_tokens": 4, "total_tokens": 35}
            }),
        )
        .await;
        let model = OpenAiCompatModel::new(
            ModelTier::System2,
            "gpt-x",
            Some("sk-oai".into()),
            Some(&url),
        )
        .unwrap();
        // The fake listens on loopback, so the endpoint counts as local.
        assert_eq!(model.info().locality, Locality::Local);
        let schema = json!({"type": "object"});
        let request = CompletionRequest::prompt(CallPurpose::Synthesize, "hi")
            .with_system("sys")
            .with_max_tokens(99)
            .with_temperature(0.5)
            .with_format(ResponseFormat::JsonSchema(schema.clone()));
        let completion = model.complete(request).await.unwrap();
        assert_eq!(completion.usage, TokenUsage::new(31, 4));
        assert!(!completion.usage_estimated);
        assert_eq!(completion.finish_reason, FinishReason::Filtered);
        assert_eq!(completion.model, "gpt-x");

        let (headers, body) = seen.lock()[0].clone();
        assert_eq!(headers["authorization"], "Bearer sk-oai");
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["max_tokens"], 99);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stream"], false);
        assert_eq!(
            body["messages"],
            json!([{"role": "system", "content": "sys"}, {"role": "user", "content": "hi"}])
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        model.probe().await.unwrap();
        assert!(!format!("{model:?}").contains("sk-oai"));
    }

    #[tokio::test]
    async fn json_mode_without_key_and_estimated_usage() {
        let (url, seen) = fake(
            StatusCode::OK,
            json!({"choices": [{"message": {"content": "abcd"}, "finish_reason": "length"}]}),
        )
        .await;
        let model = OpenAiCompatModel::new(ModelTier::System1, "local", None, Some(&url)).unwrap();
        let request = CompletionRequest::prompt(CallPurpose::Other, "12345")
            .with_format(ResponseFormat::Json);
        let completion = model.complete(request).await.unwrap();
        assert_eq!(completion.usage, TokenUsage::new(2, 1));
        assert!(completion.usage_estimated);
        assert_eq!(completion.finish_reason, FinishReason::Length);
        let (headers, body) = seen.lock()[0].clone();
        assert!(headers.get("authorization").is_none());
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert!(body.get("temperature").is_none());
    }

    #[tokio::test]
    async fn errors_are_mapped() {
        let request = || CompletionRequest::prompt(CallPurpose::Other, "x");
        let build =
            |url: &str| OpenAiCompatModel::new(ModelTier::System2, "m", None, Some(url)).unwrap();
        let (url, _) = fake(StatusCode::UNAUTHORIZED, json!({"error": "bad key"})).await;
        assert!(matches!(
            build(&url).complete(request()).await,
            Err(ModelError::Auth(_))
        ));
        let (url, _) = fake(StatusCode::TOO_MANY_REQUESTS, json!({})).await;
        assert_eq!(
            build(&url).complete(request()).await.unwrap_err(),
            ModelError::RateLimited {
                retry_after: Some(Duration::from_millis(500))
            }
        );
        let (url, _) = fake(StatusCode::BAD_GATEWAY, json!({})).await;
        let err = build(&url).complete(request()).await.unwrap_err();
        assert!(matches!(err, ModelError::Http { status: 502, .. }) && err.is_transient());
        let (url, _) = fake(StatusCode::OK, json!({"choices": []})).await;
        let err = build(&url).complete(request()).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)));
    }
}
