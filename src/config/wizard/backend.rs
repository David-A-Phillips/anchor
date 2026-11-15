//! What the wizard needs from the outside world besides the terminal: an Ollama backend (probe,
//! provision, rebuild, pull) and a fetcher for the local embedding model.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::provision::{
    EndpointStatus, OllamaManager, ProgressFn, ProvisionError, ProvisionEvent, ProvisionOptions,
    ProvisionReport, RebuildOptions, RebuildReport,
};
use crate::config::{AnchorPaths, OllamaConfig};
use crate::engine::embed::DownloadProgress;
use crate::ollama::OllamaApi;

/// Probe, provision, rebuild and pull: everything the wizard does to an Ollama installation.
#[async_trait]
pub trait OllamaBackend: Send + Sync {
    /// Asks the server at `base_url` for its version and models (never fails: an unreachable
    /// server is an [`EndpointStatus`] without a version).
    async fn probe(&self, base_url: &str) -> EndpointStatus;

    /// `true` when `config.base_url` is served by an installation that Anchor installed and
    /// manages under `paths`.
    async fn is_managed(&self, config: &OllamaConfig, paths: &AnchorPaths) -> bool;

    /// Installs (if needed), starts, fills and verifies a managed server for `config`.
    async fn provision(
        &self,
        config: &OllamaConfig,
        paths: &AnchorPaths,
        progress: ProgressFn<'_>,
    ) -> Result<ProvisionReport, ProvisionError>;

    /// Wipes and rebuilds the managed installation and verifies it.
    async fn rebuild(
        &self,
        config: &OllamaConfig,
        paths: &AnchorPaths,
        progress: ProgressFn<'_>,
    ) -> Result<RebuildReport, ProvisionError>;

    /// Pulls `model` onto the server at `base_url`.
    async fn pull_model(
        &self,
        base_url: &str,
        model: &str,
        progress: ProgressFn<'_>,
    ) -> Result<(), String>;
}

type ManagerFactory = dyn Fn(&OllamaConfig, &AnchorPaths) -> OllamaManager + Send + Sync;

/// The real backend: the provisioning engine's [`OllamaManager`]. Tests build the manager from
/// the engine's own fakes through [`ManagerBackend::with_factory`].
pub struct ManagerBackend {
    factory: Arc<ManagerFactory>,
    options: ProvisionOptions,
    probe_timeout: Duration,
}

impl Default for ManagerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagerBackend {
    /// Real processes, the real download server and default timeouts.
    pub fn new() -> Self {
        Self::with_factory(
            |config, paths| OllamaManager::new(config.clone(), paths),
            ProvisionOptions::default(),
        )
    }

    /// A backend that builds its manager with `factory` and provisions with `options`.
    pub fn with_factory(
        factory: impl Fn(&OllamaConfig, &AnchorPaths) -> OllamaManager + Send + Sync + 'static,
        options: ProvisionOptions,
    ) -> Self {
        let probe_timeout = options.probe_timeout;
        Self {
            factory: Arc::new(factory),
            options,
            probe_timeout,
        }
    }
}

fn same_url(a: &str, b: &str) -> bool {
    let norm = |url: &str| {
        crate::ollama::normalize_base_url(url)
            .unwrap_or_else(|_| url.trim_end_matches('/').to_owned())
    };
    norm(a) == norm(b)
}

#[async_trait]
impl OllamaBackend for ManagerBackend {
    async fn probe(&self, base_url: &str) -> EndpointStatus {
        let mut status = EndpointStatus {
            base_url: base_url.to_owned(),
            version: None,
            models: Vec::new(),
        };
        let Ok(api) =
            OllamaApi::new(base_url).map(|api| api.with_probe_timeout(self.probe_timeout))
        else {
            return status;
        };
        status.version = api.version().await.ok();
        if status.version.is_some() {
            status.models = api
                .list_models()
                .await
                .map(|models| models.into_iter().map(|m| m.name).collect())
                .unwrap_or_default();
        }
        status
    }

    async fn is_managed(&self, config: &OllamaConfig, paths: &AnchorPaths) -> bool {
        let status = (self.factory)(config, paths).status().await;
        status.root_marked
            && status.install.is_some()
            && status.server_alive
            && status
                .server
                .is_some_and(|server| same_url(&server.base_url, &config.base_url))
    }

    async fn provision(
        &self,
        config: &OllamaConfig,
        paths: &AnchorPaths,
        progress: ProgressFn<'_>,
    ) -> Result<ProvisionReport, ProvisionError> {
        (self.factory)(config, paths)
            .ensure_ready(&self.options, progress)
            .await
    }

    async fn rebuild(
        &self,
        config: &OllamaConfig,
        paths: &AnchorPaths,
        progress: ProgressFn<'_>,
    ) -> Result<RebuildReport, ProvisionError> {
        let options = RebuildOptions {
            provision: self.options.clone(),
            ..RebuildOptions::default()
        };
        (self.factory)(config, paths)
            .rebuild(&options, progress)
            .await
    }

    async fn pull_model(
        &self,
        base_url: &str,
        model: &str,
        progress: ProgressFn<'_>,
    ) -> Result<(), String> {
        let api = OllamaApi::new(base_url).map_err(|err| err.to_string())?;
        api.pull(model, |update| {
            progress(ProvisionEvent::Pull {
                model: model.to_owned(),
                status: update.status.clone(),
                completed: update.completed,
                total: update.total,
            });
        })
        .await
        .map_err(|err| err.to_string())
    }
}

/// Downloads the local sentence-embedding model on request.
#[async_trait]
pub trait EmbeddingFetcher: Send + Sync {
    /// Makes the files of `repo_id` available under `models_dir` and returns their directory.
    async fn fetch(
        &self,
        models_dir: &Path,
        repo_id: &str,
        progress: &(dyn Fn(DownloadProgress) + Send + Sync),
    ) -> crate::Result<PathBuf>;
}

/// Downloads from Hugging Face with the embedding engine's own downloader.
pub struct HuggingFaceFetcher;

#[async_trait]
impl EmbeddingFetcher for HuggingFaceFetcher {
    async fn fetch(
        &self,
        models_dir: &Path,
        repo_id: &str,
        progress: &(dyn Fn(DownloadProgress) + Send + Sync),
    ) -> crate::Result<PathBuf> {
        crate::engine::embed::ensure_model(models_dir, repo_id, progress).await
    }
}
