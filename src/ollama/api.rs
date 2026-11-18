//! Ollama REST API (`/api/*`) bindings.

use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::base::net::http_client_builder;

/// Failure of an Ollama API call.
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    /// The server could not be reached at all.
    #[error("Ollama is unreachable at {url}: {reason}")]
    Unreachable {
        /// Base URL that was tried.
        url: String,
        /// Transport-level reason.
        reason: String,
    },
    /// The request did not complete in time.
    #[error("Ollama request to {path} timed out")]
    Timeout {
        /// API path of the request.
        path: String,
    },
    /// The requested model is not present on the server.
    #[error("Ollama model `{0}` is not available on the server")]
    ModelNotFound(String),
    /// The server answered with an error status.
    #[error("Ollama returned HTTP {status} for {path}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// API path of the request.
        path: String,
        /// Response body, truncated.
        body: String,
    },
    /// The response body could not be decoded.
    #[error("unexpected Ollama response for {path}: {reason}")]
    Decode {
        /// API path of the request.
        path: String,
        /// What went wrong.
        reason: String,
    },
    /// The server reported an error inside a streaming response (e.g. during a pull).
    #[error("Ollama reported an error: {0}")]
    Remote(String),
    /// The base URL is malformed.
    #[error("invalid Ollama URL `{0}`")]
    InvalidUrl(String),
}

impl From<OllamaError> for crate::base::Error {
    fn from(err: OllamaError) -> Self {
        crate::base::Error::module("ollama", err)
    }
}

/// A model present on the server (`GET /api/tags`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModel {
    /// Model name including tag, e.g. `llama3.2:3b`.
    pub name: String,
    /// Size on disk in bytes.
    #[serde(default)]
    pub size: u64,
    /// Content digest.
    #[serde(default)]
    pub digest: String,
    /// Last modification time as reported by the server (RFC 3339).
    #[serde(default)]
    pub modified_at: String,
}

/// A model currently loaded in memory (`GET /api/ps`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningModel {
    /// Model name including tag.
    pub name: String,
    /// Total memory used in bytes.
    #[serde(default)]
    pub size: u64,
    /// Bytes of the model held in GPU memory; `0` means CPU only.
    #[serde(default)]
    pub size_vram: u64,
}

/// One progress event of a model pull.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullProgress {
    /// Human-readable phase (`"pulling manifest"`, `"pulling <digest>"`, `"success"`, ...).
    #[serde(default)]
    pub status: String,
    /// Layer being transferred, when applicable.
    #[serde(default)]
    pub digest: Option<String>,
    /// Size of the current layer in bytes.
    #[serde(default)]
    pub total: Option<u64>,
    /// Bytes of the current layer transferred so far.
    #[serde(default)]
    pub completed: Option<u64>,
}

/// One chat message in Ollama's format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaMessage {
    /// `"system"`, `"user"` or `"assistant"`.
    pub role: String,
    /// Message text.
    pub content: String,
}

/// Request body of `POST /api/chat` (non-streaming).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ChatRequest {
    /// Model name.
    pub model: String,
    /// Conversation, system message first when present.
    pub messages: Vec<OllamaMessage>,
    /// `"json"` or a JSON Schema object to constrain the output; omitted for free text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    /// Model options such as `temperature` and `num_predict`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    /// How long the model stays loaded after the request (e.g. `"10m"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    /// Always `false`: Anchor uses blocking completions.
    pub stream: bool,
}

/// Response body of `POST /api/chat` (non-streaming).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ChatResponse {
    /// Model that produced the answer.
    #[serde(default)]
    pub model: String,
    /// The assistant message.
    pub message: OllamaMessage,
    /// Why generation stopped (`"stop"`, `"length"`, ...).
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Prompt tokens evaluated. Absent when the prompt was fully cached.
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    /// Tokens generated.
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Total request duration in nanoseconds, as measured by the server.
    #[serde(default)]
    pub total_duration: Option<u64>,
}

/// Request body of `POST /api/generate` (non-streaming).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GenerateRequest {
    /// Model name.
    pub model: String,
    /// Prompt text.
    pub prompt: String,
    /// System prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// `"json"` or a JSON Schema object to constrain the output; omitted for free text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    /// Model options such as `temperature` and `num_predict`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    /// How long the model stays loaded after the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    /// Always `false`: Anchor uses blocking completions.
    pub stream: bool,
}

/// Response body of `POST /api/generate` (non-streaming).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GenerateResponse {
    /// Model that produced the answer.
    #[serde(default)]
    pub model: String,
    /// Generated text.
    #[serde(default)]
    pub response: String,
    /// Why generation stopped.
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Prompt tokens evaluated.
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    /// Tokens generated.
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Total request duration in nanoseconds, as measured by the server.
    #[serde(default)]
    pub total_duration: Option<u64>,
}

/// Response body of `POST /api/embed`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EmbedResponse {
    /// Model that produced the embeddings.
    #[serde(default)]
    pub model: String,
    /// One vector per input, in input order.
    pub embeddings: Vec<Vec<f32>>,
    /// Prompt tokens evaluated.
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
}

/// Normalises a user-supplied Ollama address into a base URL without a trailing slash.
///
/// Accepts `host`, `host:port`, `http://host:port/` and the `OLLAMA_HOST` conventions: a missing
/// scheme means `http`, a missing port means `11434`, and the bind-all addresses `0.0.0.0`/`::`
/// are rewritten to loopback because they are not connectable.
pub fn normalize_base_url(input: &str) -> Result<String, OllamaError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(OllamaError::InvalidUrl(input.to_owned()));
    }
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let mut url =
        reqwest::Url::parse(&with_scheme).map_err(|_| OllamaError::InvalidUrl(input.to_owned()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(OllamaError::InvalidUrl(input.to_owned()));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| OllamaError::InvalidUrl(input.to_owned()))?
        .to_owned();
    if host == "0.0.0.0" || host == "[::]" {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| OllamaError::InvalidUrl(input.to_owned()))?;
    }
    if url.port().is_none() && url.scheme() == "http" {
        url.set_port(Some(11434))
            .map_err(|()| OllamaError::InvalidUrl(input.to_owned()))?;
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Adds the implicit `:latest` tag to a model name that has none.
pub fn normalize_model_name(name: &str) -> String {
    let name = name.trim();
    let last_segment = name.rsplit('/').next().unwrap_or(name);
    if last_segment.contains(':') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    }
}

/// Client for one Ollama server.
#[derive(Clone, Debug)]
pub struct OllamaApi {
    base_url: String,
    http: reqwest::Client,
    probe_timeout: Duration,
}

/// Default deadline of the liveness probe. Deliberately short: on some hosts (WSL2 among them) a
/// connection to a closed loopback port is silently dropped instead of refused, so an absent server
/// is only ever detected by this timeout.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const ADMIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect deadline for a server on this machine. A loopback connection opens in well under a
/// millisecond when something listens; waiting longer only delays the fallback (and every
/// question) when nothing does, which matters most where closed ports are dropped, not refused.
const LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// `true` when `base_url` names this machine.
fn is_loopback(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl OllamaApi {
    /// Creates a client for the server at `base_url` (see [`normalize_base_url`]).
    pub fn new(base_url: &str) -> Result<Self, OllamaError> {
        let base_url = normalize_base_url(base_url)?;
        let mut builder = http_client_builder();
        if is_loopback(&base_url) {
            builder = builder.connect_timeout(LOOPBACK_CONNECT_TIMEOUT);
        }
        let http = builder.build().map_err(|err| OllamaError::Unreachable {
            url: base_url.clone(),
            reason: err.to_string(),
        })?;
        Ok(Self {
            base_url,
            http,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        })
    }

    /// Sets the deadline of the liveness probe ([`Self::version`], [`Self::is_reachable`]).
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Normalised base URL, without trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn transport_error(&self, path: &str, err: reqwest::Error) -> OllamaError {
        if err.is_timeout() {
            OllamaError::Timeout {
                path: path.to_owned(),
            }
        } else if err.is_decode() {
            OllamaError::Decode {
                path: path.to_owned(),
                reason: err.to_string(),
            }
        } else {
            OllamaError::Unreachable {
                url: self.base_url.clone(),
                reason: err.to_string(),
            }
        }
    }

    async fn check(
        &self,
        path: &str,
        model: Option<&str>,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, OllamaError> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let mut body = response.text().await.unwrap_or_default();
        if body.len() > 2_000 {
            let mut cut = 2_000;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
        }
        if status.as_u16() == 404
            && let Some(model) = model
        {
            return Err(OllamaError::ModelNotFound(model.to_owned()));
        }
        Err(OllamaError::Status {
            status: status.as_u16(),
            path: path.to_owned(),
            body,
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<T, OllamaError> {
        let response = self
            .http
            .get(self.url(path))
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| self.transport_error(path, err))?;
        let response = self.check(path, None, response).await?;
        response
            .json::<T>()
            .await
            .map_err(|err| self.transport_error(path, err))
    }

    async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        model: &str,
        body: &B,
        timeout: Option<Duration>,
    ) -> Result<T, OllamaError> {
        let mut request = self.http.post(self.url(path)).json(body);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request
            .send()
            .await
            .map_err(|err| self.transport_error(path, err))?;
        let response = self.check(path, Some(model), response).await?;
        response
            .json::<T>()
            .await
            .map_err(|err| self.transport_error(path, err))
    }

    /// Server version (`GET /api/version`). The cheapest liveness probe.
    pub async fn version(&self) -> Result<String, OllamaError> {
        #[derive(Deserialize)]
        struct Version {
            version: String,
        }
        let version: Version = self.get_json("/api/version", self.probe_timeout).await?;
        Ok(version.version)
    }

    /// `true` when the server answers the version probe.
    pub async fn is_reachable(&self) -> bool {
        self.version().await.is_ok()
    }

    /// Models present on the server (`GET /api/tags`).
    pub async fn list_models(&self) -> Result<Vec<LocalModel>, OllamaError> {
        #[derive(Deserialize)]
        struct Tags {
            #[serde(default)]
            models: Vec<LocalModel>,
        }
        let tags: Tags = self.get_json("/api/tags", ADMIN_TIMEOUT).await?;
        Ok(tags.models)
    }

    /// `true` when `model` (with the implicit `:latest` tag applied) is present on the server.
    pub async fn has_model(&self, model: &str) -> Result<bool, OllamaError> {
        let wanted = normalize_model_name(model);
        Ok(self
            .list_models()
            .await?
            .iter()
            .any(|local| normalize_model_name(&local.name) == wanted))
    }

    /// Models currently loaded in memory (`GET /api/ps`).
    pub async fn running_models(&self) -> Result<Vec<RunningModel>, OllamaError> {
        #[derive(Deserialize)]
        struct Ps {
            #[serde(default)]
            models: Vec<RunningModel>,
        }
        let ps: Ps = self.get_json("/api/ps", ADMIN_TIMEOUT).await?;
        Ok(ps.models)
    }

    /// Downloads `model` (`POST /api/pull`), reporting progress events as they arrive. Returns once
    /// the server reports success; a pull of a model that is already present succeeds quickly.
    pub async fn pull(
        &self,
        model: &str,
        mut on_progress: impl FnMut(&PullProgress) + Send,
    ) -> Result<(), OllamaError> {
        const PATH: &str = "/api/pull";
        let body = serde_json::json!({ "model": model, "stream": true });
        let response = self
            .http
            .post(self.url(PATH))
            .json(&body)
            .send()
            .await
            .map_err(|err| self.transport_error(PATH, err))?;
        let response = self.check(PATH, None, response).await?;

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut succeeded = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| self.transport_error(PATH, err))?;
            buffer.extend_from_slice(&chunk);
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                succeeded |= handle_pull_line(&line, &mut on_progress)?;
            }
        }
        if !buffer.is_empty() {
            succeeded |= handle_pull_line(&buffer, &mut on_progress)?;
        }
        if succeeded {
            Ok(())
        } else {
            Err(OllamaError::Remote(format!(
                "pull of `{model}` ended without a success status"
            )))
        }
    }

    /// Removes `model` from the server (`DELETE /api/delete`). Deleting a model that is not
    /// present is reported as [`OllamaError::ModelNotFound`].
    pub async fn delete_model(&self, model: &str) -> Result<(), OllamaError> {
        const PATH: &str = "/api/delete";
        let body = serde_json::json!({ "model": model });
        let response = self
            .http
            .delete(self.url(PATH))
            .json(&body)
            .timeout(ADMIN_TIMEOUT)
            .send()
            .await
            .map_err(|err| self.transport_error(PATH, err))?;
        self.check(PATH, Some(model), response).await?;
        Ok(())
    }

    /// Chat completion (`POST /api/chat`), non-streaming.
    pub async fn chat(
        &self,
        request: &ChatRequest,
        timeout: Option<Duration>,
    ) -> Result<ChatResponse, OllamaError> {
        let mut request = request.clone();
        request.stream = false;
        let model = request.model.clone();
        self.post_json("/api/chat", &model, &request, timeout).await
    }

    /// Text completion (`POST /api/generate`), non-streaming.
    pub async fn generate(
        &self,
        request: &GenerateRequest,
        timeout: Option<Duration>,
    ) -> Result<GenerateResponse, OllamaError> {
        let mut request = request.clone();
        request.stream = false;
        let model = request.model.clone();
        self.post_json("/api/generate", &model, &request, timeout)
            .await
    }

    /// Embeds `inputs` with `model` (`POST /api/embed`).
    pub async fn embed(
        &self,
        model: &str,
        inputs: &[String],
        timeout: Option<Duration>,
    ) -> Result<EmbedResponse, OllamaError> {
        let body = serde_json::json!({ "model": model, "input": inputs });
        let response: EmbedResponse = self.post_json("/api/embed", model, &body, timeout).await?;
        if response.embeddings.len() != inputs.len() {
            return Err(OllamaError::Decode {
                path: "/api/embed".to_owned(),
                reason: format!(
                    "expected {} embeddings, received {}",
                    inputs.len(),
                    response.embeddings.len()
                ),
            });
        }
        Ok(response)
    }
}

/// Parses one NDJSON line of a pull stream. Returns `true` when the line reports success.
fn handle_pull_line(
    line: &[u8],
    on_progress: &mut (impl FnMut(&PullProgress) + Send),
) -> Result<bool, OllamaError> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return Ok(false);
    }
    let value: Value = serde_json::from_str(text).map_err(|err| OllamaError::Decode {
        path: "/api/pull".to_owned(),
        reason: err.to_string(),
    })?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(OllamaError::Remote(error.to_owned()));
    }
    let progress: PullProgress =
        serde_json::from_value(value).map_err(|err| OllamaError::Decode {
            path: "/api/pull".to_owned(),
            reason: err.to_string(),
        })?;
    on_progress(&progress);
    Ok(progress.status == "success")
}

#[cfg(test)]
mod tests {
    #[test]
    fn loopback_hosts_are_recognised() {
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "http://[::1]:11434",
            "http://127.1.2.3",
        ] {
            assert!(super::is_loopback(url), "{url}");
        }
        for url in [
            "http://10.0.0.5:11434",
            "https://ollama.example.com",
            "not a url",
        ] {
            assert!(!super::is_loopback(url), "{url}");
        }
    }

    use super::*;

    #[test]
    fn base_urls_are_normalised() {
        let cases = [
            ("localhost", "http://localhost:11434"),
            ("localhost:11434/", "http://localhost:11434"),
            ("http://gpu-box:8080", "http://gpu-box:8080"),
            ("0.0.0.0:11434", "http://127.0.0.1:11434"),
            ("0.0.0.0", "http://127.0.0.1:11434"),
            ("https://ollama.example.com", "https://ollama.example.com"),
            ("  http://127.0.0.1:11434//  ", "http://127.0.0.1:11434"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_base_url(input).expect(input), expected, "{input}");
        }
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("ftp://host").is_err());
        assert!(normalize_base_url("http://").is_err());
    }

    #[test]
    fn model_names_get_an_implicit_latest_tag() {
        assert_eq!(normalize_model_name("llama3.2"), "llama3.2:latest");
        assert_eq!(normalize_model_name("llama3.2:3b"), "llama3.2:3b");
        assert_eq!(
            normalize_model_name("registry.local:5000/team/model"),
            "registry.local:5000/team/model:latest"
        );
        assert_eq!(
            normalize_model_name("registry.local:5000/team/model:q4"),
            "registry.local:5000/team/model:q4"
        );
    }

    #[test]
    fn pull_lines_report_progress_success_and_errors() {
        let mut seen = Vec::new();
        let mut record = |progress: &PullProgress| seen.push(progress.clone());
        assert!(
            !handle_pull_line(
                br#"{"status":"pulling abc","digest":"sha256:abc","total":10,"completed":4}"#,
                &mut record
            )
            .expect("progress line")
        );
        assert!(!handle_pull_line(b"   \n", &mut record).expect("blank line"));
        assert!(handle_pull_line(br#"{"status":"success"}"#, &mut record).expect("success line"));
        assert!(matches!(
            handle_pull_line(
                br#"{"error":"pull model manifest: file does not exist"}"#,
                &mut record
            ),
            Err(OllamaError::Remote(_))
        ));
        assert!(handle_pull_line(b"not json", &mut record).is_err());
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].completed, Some(4));
        assert_eq!(seen[1].status, "success");
    }

    #[tokio::test]
    async fn unreachable_server_is_reported_as_such() {
        // Port 9 (discard) on loopback is closed. Depending on the host the connection is either
        // refused or silently dropped, hence the short probe timeout.
        let api = OllamaApi::new("http://127.0.0.1:9")
            .expect("client")
            .with_probe_timeout(Duration::from_millis(250));
        assert!(!api.is_reachable().await);
        assert!(matches!(
            api.version().await,
            Err(OllamaError::Unreachable { .. } | OllamaError::Timeout { .. })
        ));
    }
}
