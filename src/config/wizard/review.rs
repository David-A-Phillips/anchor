//! Embeddings and the final review (summary, validation, confirmation, atomic save).

use crate::base::ModelTier;
use crate::config::{AnchorConfig, Issue};
use crate::engine::embed::{
    DEFAULT_CANDLE_MODEL, EmbeddingBackend, model_dir, model_files_present,
};

use super::flow::{Session, Step, Stop};
use super::validator::{TierValidation, provider_name};
use super::{Choice, Progress};

/// Default Ollama embedding model suggested by the wizard.
const OLLAMA_EMBEDDING_MODEL: &str = "nomic-embed-text";

fn trust_name(trust: crate::base::TrustLevel) -> &'static str {
    match trust {
        crate::base::TrustLevel::SourceOfTruth => "SourceOfTruth",
        crate::base::TrustLevel::ReferenceOnly => "ReferenceOnly",
    }
}

/// The complete configuration as human-readable lines for the review step.
pub fn summarize(config: &AnchorConfig, validations: &[TierValidation]) -> Vec<String> {
    let mut lines = vec![format!("Mode: {:?}", config.mode)];
    let ollama = &config.ollama;
    lines.push(format!(
        "Ollama: {} ({}, auto-provision {})",
        ollama.base_url,
        if ollama.managed {
            "managed by Anchor"
        } else {
            "external"
        },
        if ollama.auto_provision { "on" } else { "off" }
    ));
    for (label, tier, kind) in [
        ("System 1", &config.models.system1, ModelTier::System1),
        ("System 2", &config.models.system2, ModelTier::System2),
    ] {
        if !tier.is_enabled() {
            lines.push(format!(
                "{label}: {}",
                if kind == ModelTier::System1 {
                    "heuristic only (no model)"
                } else {
                    "disabled"
                }
            ));
            continue;
        }
        let status = validations
            .iter()
            .find(|v| v.tier == kind && v.config == *tier)
            .map_or("NOT VALIDATED".to_owned(), |v| {
                if v.ok {
                    format!("validated, {} ms", v.latency.map_or(0, |l| l.as_millis()))
                } else {
                    "FAILED".to_owned()
                }
            });
        let key = tier
            .api_key_env
            .as_deref()
            .map_or(String::new(), |name| format!(", key from ${name}"));
        lines.push(format!(
            "{label}: {} `{}`{key} [{status}]",
            provider_name(tier.provider),
            tier.model
        ));
    }
    let routing = &config.models.routing;
    lines.push(format!(
        "Routing: cloud {}{}",
        if routing.allow_cloud {
            "allowed"
        } else {
            "off"
        },
        routing
            .max_cloud_usd_per_day
            .map_or(String::new(), |b| format!(", budget ${b}/day"))
    ));
    lines.push(format!(
        "Embeddings: {:?}{}",
        config.embeddings.backend,
        if config.embeddings.model.is_empty() {
            String::new()
        } else {
            format!(" `{}`", config.embeddings.model)
        }
    ));
    if config.sources.is_empty() {
        lines.push("Sources: none yet (add [[sources]] or re-run `anchor setup`)".to_owned());
    }
    for source in &config.sources {
        lines.push(format!(
            "Source {} [{}] {} -> {}",
            source.id,
            source.kind,
            trust_name(source.trust),
            source.location
        ));
    }
    lines.push(format!(
        "Data directory: {}",
        config.resolved_data_dir().display()
    ));
    lines
}

impl Session<'_> {
    pub(super) async fn step_embeddings(&mut self) -> Step {
        let mut backends = vec![EmbeddingBackend::Hash];
        let mut choices = vec![Choice::with_help(
            "Hash",
            "no download, works everywhere; lexical quality",
        )];
        if cfg!(feature = "embed-candle") {
            backends.push(EmbeddingBackend::Candle);
            choices.push(Choice::with_help(
                "Candle MiniLM",
                "local sentence embeddings; downloads about 90 MB on request",
            ));
        }
        backends.push(EmbeddingBackend::Ollama);
        choices.push(Choice::with_help(
            "Ollama embedding model",
            "served by your Ollama",
        ));
        let current = self.config.embeddings.backend;
        let default = backends.iter().position(|b| *b == current).unwrap_or(0);
        let backend = backends[self.p.select("Embeddings backend", &choices, default)?];
        let same = backend == current;
        match backend {
            EmbeddingBackend::Hash => {
                self.config.embeddings.backend = backend;
            }
            EmbeddingBackend::Candle => {
                let model_default = if same && !self.config.embeddings.model.is_empty() {
                    self.config.embeddings.model.clone()
                } else {
                    DEFAULT_CANDLE_MODEL.to_owned()
                };
                let model = self.ask_valid(
                    "Candle model (Hugging Face repository)",
                    Some(&model_default),
                    |text| {
                        if text.is_empty() {
                            Err("a model is required".to_owned())
                        } else {
                            Ok(text.to_owned())
                        }
                    },
                )?;
                self.config.embeddings.backend = backend;
                self.config.embeddings.model.clone_from(&model);
                self.fetch_candle(&model).await?;
            }
            EmbeddingBackend::Ollama => {
                let model_default = if same && !self.config.embeddings.model.is_empty() {
                    self.config.embeddings.model.clone()
                } else {
                    OLLAMA_EMBEDDING_MODEL.to_owned()
                };
                let model =
                    self.ask_valid("Ollama embedding model", Some(&model_default), |text| {
                        if text.is_empty() {
                            Err("a model is required".to_owned())
                        } else {
                            Ok(text.to_owned())
                        }
                    })?;
                self.config.embeddings.backend = backend;
                self.config.embeddings.model.clone_from(&model);
                if !self.ollama_available {
                    self.p.warn("No Ollama server is available now; the embedding model must exist on the server before Anchor starts.");
                } else if !self
                    .models
                    .iter()
                    .any(|m| m == &model || m.strip_suffix(":latest") == Some(model.as_str()))
                    && self
                        .p
                        .confirm(&format!("{model} is not on the server. Pull it now?"), true)?
                {
                    self.pull(&model).await;
                }
            }
        }
        Ok(())
    }

    async fn fetch_candle(&mut self, repo: &str) -> Step {
        let models_dir = self.paths().models_dir();
        let present = model_dir(&models_dir, repo)
            .map(|dir| model_files_present(&dir))
            .unwrap_or(false);
        if present {
            self.p.note("The embedding model is already downloaded.");
            return Ok(());
        }
        if !self.p.confirm(
            "Download the embedding model now? Anchor never downloads it on its own.",
            false,
        )? {
            self.p.warn("The embedding model is not downloaded yet: the engine cannot start with the Candle backend until it is. Re-run setup and accept the download, or switch to the hash backend.");
            return Ok(());
        }
        let p = self.p;
        let sink = move |update: crate::engine::embed::DownloadProgress| {
            p.progress(Progress::Bytes {
                label: format!("download {}", update.file),
                done: update.downloaded,
                total: update.total,
            });
        };
        let result = self.env.embeddings.fetch(&models_dir, repo, &sink).await;
        self.p.progress(Progress::Done);
        match result {
            Ok(dir) => self
                .p
                .note(&format!("Embedding model ready in {}.", dir.display())),
            Err(err) => {
                self.p.warn(&format!(
                    "The embedding model download failed ({err}); falling back to the hash backend."
                ));
                self.config.embeddings.backend = EmbeddingBackend::Hash;
                self.config.embeddings.model.clear();
            }
        }
        Ok(())
    }

    pub(super) async fn step_review(&mut self) -> Step<Vec<Issue>> {
        // Gating is enforced here again: no enabled tier may be saved without a passing validation
        // of exactly the configuration being saved.
        for (kind, tier) in [
            (ModelTier::System1, &self.config.models.system1),
            (ModelTier::System2, &self.config.models.system2),
        ] {
            if tier.is_enabled()
                && !self
                    .validations
                    .iter()
                    .any(|v| v.ok && v.tier == kind && v.config == *tier)
            {
                return Err(Stop::Fail(crate::base::Error::config(format!(
                    "{kind} is enabled but has not passed endpoint validation; nothing was saved"
                ))));
            }
        }
        if self.config.mode == crate::config::Mode::Client && !self.config.sources.is_empty() {
            self.choose_published()?;
        }
        self.p.note("Review:");
        for line in summarize(&self.config, &self.validations) {
            self.p.note(&format!("  {line}"));
        }
        let report = self.config.validate();
        for warning in &report.warnings {
            self.p.warn(&warning.to_string());
        }
        if !report.errors.is_empty() {
            for error in &report.errors {
                self.p.warn(&format!("error: {error}"));
            }
            return Err(Stop::Fail(crate::base::Error::config(format!(
                "the configuration has {} error(s); nothing was saved",
                report.errors.len()
            ))));
        }
        let path = self.env.config_path.clone();
        if !self
            .p
            .confirm(&format!("Save to {}?", path.display()), true)?
        {
            return Err(Stop::Cancelled);
        }
        let config = self.config.clone();
        let target = path.clone();
        tokio::task::spawn_blocking(move || -> crate::Result<()> {
            config.save(&target)?;
            config.paths().ensure()
        })
        .await
        .map_err(|err| crate::base::Error::other(err.to_string()))??;
        self.p.note(&format!("Saved {}.", path.display()));
        if self.config.sources.iter().any(|s| s.enabled) {
            self.p.note("Next: `anchor ingest`, then `anchor daemon`.");
        }
        Ok(report.warnings)
    }

    fn choose_published(&mut self) -> Step {
        let ids: Vec<String> = self
            .config
            .sources
            .iter()
            .map(|s| s.id.to_string())
            .collect();
        let choices: Vec<Choice> = ids.iter().map(Choice::new).collect();
        let defaults: Vec<usize> = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                self.config
                    .sync
                    .publish
                    .iter()
                    .any(|p| p.as_str() == id.as_str())
            })
            .map(|(i, _)| i)
            .collect();
        let picked = self.p.multi_select(
            "Which sources should this client publish to the server?",
            &choices,
            &defaults,
        )?;
        self.config.sync.publish = picked
            .into_iter()
            .map(|i| crate::base::SourceId::new(ids[i].clone()))
            .collect();
        Ok(())
    }
}
