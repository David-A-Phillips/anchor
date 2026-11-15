//! A programmable in-memory model for tests in this and other modules.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;

use super::provider::HealthProbe;
use crate::base::{
    Completion, CompletionRequest, FinishReason, LanguageModel, Locality, ModelError, ModelInfo,
    ModelTier, TokenUsage,
};

/// What a [`ScriptedModel`] answers to one request.
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptedReply {
    /// Succeeds with this text; usage is estimated from the text lengths.
    Text(String),
    /// Succeeds with this text and exactly this (provider-reported) usage.
    TextWithUsage(String, TokenUsage),
    /// Fails with this error.
    Error(ModelError),
}

#[derive(Clone, Debug)]
struct Step {
    reply: ScriptedReply,
    delay: Duration,
}

/// A [`LanguageModel`] that replays queued replies in order and records every request.
///
/// Replies queued with the `push_*` methods are consumed first-in first-out; once the queue is
/// empty the reply set with [`ScriptedModel::always`] is used, and without one the call fails with
/// `ModelError::Other("scripted model has no reply left")`.
#[derive(Debug)]
pub struct ScriptedModel {
    info: ModelInfo,
    queue: Mutex<VecDeque<Step>>,
    fallback: Mutex<Option<Step>>,
    requests: Mutex<Vec<CompletionRequest>>,
    healthy: Mutex<bool>,
}

impl ScriptedModel {
    /// Creates a model with an empty script. Provider is `"scripted"`, model `"scripted"`.
    pub fn new(tier: ModelTier, locality: Locality) -> Self {
        Self {
            info: ModelInfo {
                tier,
                locality,
                provider: "scripted".to_owned(),
                model: "scripted".to_owned(),
            },
            queue: Mutex::default(),
            fallback: Mutex::default(),
            requests: Mutex::default(),
            healthy: Mutex::new(true),
        }
    }

    /// Queues a reply.
    pub fn push(&self, reply: ScriptedReply) -> &Self {
        self.push_delayed(Duration::ZERO, reply)
    }

    /// Queues a reply delivered after `delay` (for timeout tests).
    pub fn push_delayed(&self, delay: Duration, reply: ScriptedReply) -> &Self {
        self.queue.lock().push_back(Step { reply, delay });
        self
    }

    /// Queues a successful text reply.
    pub fn push_text(&self, text: impl Into<String>) -> &Self {
        self.push(ScriptedReply::Text(text.into()))
    }

    /// Queues a failure.
    pub fn push_error(&self, error: ModelError) -> &Self {
        self.push(ScriptedReply::Error(error))
    }

    /// Sets the reply used whenever the queue is empty.
    pub fn always(&self, reply: ScriptedReply) -> &Self {
        *self.fallback.lock() = Some(Step {
            reply,
            delay: Duration::ZERO,
        });
        self
    }

    /// Makes the health probe succeed or fail.
    pub fn set_healthy(&self, healthy: bool) -> &Self {
        *self.healthy.lock() = healthy;
        self
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().clone()
    }

    /// Number of requests received so far.
    pub fn calls(&self) -> usize {
        self.requests.lock().len()
    }
}

#[async_trait]
impl LanguageModel for ScriptedModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, ModelError> {
        let started = Instant::now();
        let prompt_chars = request.prompt_chars();
        self.requests.lock().push(request);
        let step = self
            .queue
            .lock()
            .pop_front()
            .or_else(|| self.fallback.lock().clone())
            .ok_or_else(|| ModelError::Other("scripted model has no reply left".to_owned()))?;
        if !step.delay.is_zero() {
            tokio::time::sleep(step.delay).await;
        }
        let (text, usage, usage_estimated) = match step.reply {
            ScriptedReply::Text(text) => {
                let usage = TokenUsage::estimate(prompt_chars, text.len());
                (text, usage, true)
            }
            ScriptedReply::TextWithUsage(text, usage) => (text, usage, false),
            ScriptedReply::Error(error) => return Err(error),
        };
        Ok(Completion {
            text,
            usage,
            usage_estimated,
            latency: started.elapsed(),
            model: self.info.model.clone(),
            finish_reason: FinishReason::Stop,
        })
    }
}

#[async_trait]
impl HealthProbe for ScriptedModel {
    async fn probe(&self) -> Result<(), ModelError> {
        if *self.healthy.lock() {
            Ok(())
        } else {
            Err(ModelError::Unavailable(
                "scripted model marked unhealthy".to_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::CallPurpose;

    #[tokio::test]
    async fn replays_script_then_fallback_and_records_requests() {
        let model = ScriptedModel::new(ModelTier::System1, Locality::Local);
        model
            .push_text("one")
            .push_error(ModelError::Auth("no".into()))
            .push(ScriptedReply::TextWithUsage(
                "three".into(),
                TokenUsage::new(5, 6),
            ));
        let request = || CompletionRequest::prompt(CallPurpose::Other, "abcd");
        let first = model.complete(request()).await.unwrap();
        assert_eq!((first.text.as_str(), first.usage_estimated), ("one", true));
        assert_eq!(first.usage, TokenUsage::new(1, 1));
        assert_eq!(
            model.complete(request()).await,
            Err(ModelError::Auth("no".into()))
        );
        let third = model.complete(request()).await.unwrap();
        assert_eq!(third.usage, TokenUsage::new(5, 6));
        assert!(model.complete(request()).await.is_err());
        model.always(ScriptedReply::Text("again".into()));
        assert_eq!(model.complete(request()).await.unwrap().text, "again");
        assert_eq!(model.calls(), 5);
        assert_eq!(model.requests()[0].messages[0].content, "abcd");
        assert!(model.probe().await.is_ok());
        assert!(model.set_healthy(false).probe().await.is_err());
    }
}
