//! Typed client for the Ollama HTTP API.
//!
//! This is the single place that knows Ollama's wire format. It is used by the provisioning code
//! (`config::provision`: health checks, pulling and deleting models), by the System 1 model
//! provider (`engine::router`) and by the Ollama embedding backend (`engine::embed`).

mod api;

pub use api::{
    ChatRequest, ChatResponse, EmbedResponse, GenerateRequest, GenerateResponse, LocalModel,
    OllamaApi, OllamaError, OllamaMessage, PullProgress, RunningModel, normalize_base_url,
    normalize_model_name,
};

/// Default address of a local Ollama server.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";
