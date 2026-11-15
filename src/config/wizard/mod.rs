//! The interactive setup wizard (`anchor setup`).
//!
//! The wizard walks through operating mode, Ollama (detecting, installing and managing it through
//! [`crate::config::provision`]), an optional bootstrap-resilience (wipe and rebuild) test, the
//! System 1 / System 2 model tiers with endpoint validation gating, sources with explicit trust
//! levels, embeddings, and a final review before an atomic save.
//!
//! Everything it does to the outside world goes through an injectable seam:
//!
//! * [`Prompter`]: the terminal ([`InquirePrompter`]), a script ([`ScriptedPrompter`]) or
//!   "accept all defaults" ([`DefaultsPrompter`], used for `--yes`).
//! * [`OllamaBackend`]: probing, provisioning, rebuilding and pulling ([`ManagerBackend`] drives the
//!   real provisioning engine).
//! * [`EndpointValidator`]: a real tiny completion through the model router ([`ModelValidator`]).
//! * [`EmbeddingFetcher`]: the local embedding model download.
//!
//! API keys are never typed into or stored by the wizard: it asks for the *name* of the
//! environment variable and checks that it is set.

mod backend;
mod flow;
mod prompter;
mod review;
mod sources;
#[cfg(test)]
mod tests;
mod tiers;
mod validator;

use std::path::PathBuf;
use std::sync::Arc;

use crate::base::Result;
use crate::config::provision::{ProvisionReport, RebuildReport};
use crate::config::{AnchorConfig, Issue, Mode, SourceConfig};
use crate::engine::router::TierConfig;

pub use backend::{EmbeddingFetcher, HuggingFaceFetcher, ManagerBackend, OllamaBackend};
pub use prompter::{
    Answer, Asked, Choice, DefaultsPrompter, InquirePrompter, Progress, PromptDefault, PromptError,
    PromptResult, Prompter, ScriptedPrompter,
};
pub use review::summarize;
pub use sources::{discover_repositories, slugify};
pub use validator::{
    EndpointValidator, EnvFn, ModelFactory, ModelValidator, TierValidation, provider_name,
};

/// Preselected answers and switches for scripted (non-interactive) use.
#[derive(Clone, Debug, Default)]
pub struct WizardOptions {
    /// Accept the default of every question (`--yes`). Anything without a default must be
    /// supplied through the other options, or the wizard stops with an error.
    pub yes: bool,
    /// Operating mode; skips the question.
    pub mode: Option<Mode>,
    /// Sources to add (with their trust levels) without asking.
    pub sources: Vec<SourceConfig>,
    /// Model for System 1 on the Ollama server; skips the question (pulled if missing).
    pub system1_model: Option<String>,
    /// Complete System 2 configuration; skips the question.
    pub system2: Option<TierConfig>,
    /// Never install or start anything: an unreachable Ollama means heuristic-only System 1.
    pub skip_provisioning: bool,
    /// Run the bootstrap resilience test (wipe, remove, rebuild) without asking. Refused when the
    /// Ollama server is not managed by Anchor.
    pub rebuild_test: bool,
}

/// Where the wizard runs and what it talks to.
#[derive(Clone)]
pub struct WizardEnvironment {
    /// The configuration file to write.
    pub config_path: PathBuf,
    /// The configuration as it is on disk now, if there is one; every prompt is pre-filled from it.
    pub existing: Option<AnchorConfig>,
    /// Data directory to use when the configuration does not name one (`None`: the default).
    pub data_dir: Option<PathBuf>,
    /// Environment-variable lookup (API key checks, `HOME`).
    pub env: EnvFn,
    /// Ollama probing and provisioning.
    pub ollama: Arc<dyn OllamaBackend>,
    /// Endpoint validation.
    pub validator: Arc<dyn EndpointValidator>,
    /// Embedding model download.
    pub embeddings: Arc<dyn EmbeddingFetcher>,
}

impl WizardEnvironment {
    /// The real environment for the configuration file at `config_path`: the process
    /// environment, the real provisioning engine, the model router and Hugging Face.
    pub fn real(config_path: PathBuf, existing: Option<AnchorConfig>) -> Self {
        Self {
            config_path,
            existing,
            data_dir: None,
            env: Arc::new(crate::engine::router::process_env),
            ollama: Arc::new(ManagerBackend::new()),
            validator: Arc::new(ModelValidator::router()),
            embeddings: Arc::new(HuggingFaceFetcher),
        }
    }

    /// The configuration that pre-fills the prompts: the existing one, or the defaults.
    pub fn starting_config(&self) -> AnchorConfig {
        self.existing.clone().unwrap_or_default()
    }

    pub(crate) fn lookup(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }
}

/// Result of the bootstrap resilience test.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RebuildTest {
    /// Per-phase report, when the rebuild completed.
    pub report: Option<RebuildReport>,
    /// Why the test was refused (an external server is never wiped).
    pub refused: Option<String>,
    /// Why the test failed, when it did.
    pub error: Option<String>,
}

/// What the wizard did.
#[derive(Clone, Debug)]
pub struct WizardOutcome {
    /// The configuration as it was when the wizard ended (saved unless cancelled).
    pub config: AnchorConfig,
    /// Where it was saved; `None` when the user cancelled.
    pub saved: Option<PathBuf>,
    /// `true` when the user cancelled; the configuration file is then untouched.
    pub cancelled: bool,
    /// What provisioning did, when the wizard provisioned Ollama.
    pub provisioning: Option<ProvisionReport>,
    /// Validation result of every enabled tier.
    pub validations: Vec<TierValidation>,
    /// The bootstrap resilience test, when it was requested or offered and taken.
    pub rebuild_test: Option<RebuildTest>,
    /// Warnings of the final configuration validation.
    pub warnings: Vec<Issue>,
}

/// Runs the wizard.
///
/// A cancelled run is `Ok` with [`WizardOutcome::cancelled`] set; failures (a failing validation in
/// `--yes` mode, an invalid configuration, a missing default, I/O) are errors. In every case the
/// configuration file is written only after validation succeeded, atomically, at the very end.
pub async fn run_wizard(
    prompter: &dyn Prompter,
    env: &WizardEnvironment,
    options: &WizardOptions,
) -> Result<WizardOutcome> {
    flow::run(prompter, env, options).await
}
