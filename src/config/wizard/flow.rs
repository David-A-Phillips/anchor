//! The wizard's flow: mode, Ollama, rebuild test, tiers, sources (see `sources.rs`), embeddings
//! and review.

use crate::base::{Error, ModelTier, Result};
use crate::config::file::is_env_name;
use crate::config::provision::{ProvisionEvent, ProvisionReport, ServerMode};
use crate::config::{AnchorConfig, AnchorPaths, Mode};
use crate::engine::router::{ProviderKind, TierConfig};
use crate::ollama::normalize_base_url;
use crate::sync::SyncTransport;

use super::{
    Choice, DefaultsPrompter, Progress, PromptError, Prompter, RebuildTest, TierValidation,
    WizardEnvironment, WizardOptions, WizardOutcome,
};

/// Why a step stopped the wizard.
pub(super) enum Stop {
    /// The user cancelled.
    Cancelled,
    /// Something failed.
    Fail(Error),
}

impl From<PromptError> for Stop {
    fn from(err: PromptError) -> Self {
        match err {
            PromptError::Cancelled => Self::Cancelled,
            other => Self::Fail(Error::invalid(other.to_string())),
        }
    }
}

impl From<Error> for Stop {
    fn from(err: Error) -> Self {
        Self::Fail(err)
    }
}

pub(super) type Step<T = ()> = std::result::Result<T, Stop>;

pub(super) struct Session<'a> {
    pub(super) p: &'a dyn Prompter,
    pub(super) env: &'a WizardEnvironment,
    pub(super) opts: &'a WizardOptions,
    pub(super) config: AnchorConfig,
    /// A reachable Ollama server is available for local models.
    pub(super) ollama_available: bool,
    /// Models present on that server.
    pub(super) models: Vec<String>,
    /// That server is installed and managed by Anchor.
    pub(super) managed: bool,
    pub(super) provisioning: Option<ProvisionReport>,
    pub(super) validations: Vec<TierValidation>,
    pub(super) rebuild: Option<RebuildTest>,
    /// The preselected System 1 model could not be pulled; ask instead.
    system1_preset_failed: bool,
}

pub(super) async fn run(
    prompter: &dyn Prompter,
    env: &WizardEnvironment,
    options: &WizardOptions,
) -> Result<WizardOutcome> {
    let defaults;
    let p: &dyn Prompter = if options.yes {
        defaults = DefaultsPrompter::new(prompter);
        &defaults
    } else {
        prompter
    };
    let mut config = env.starting_config();
    if config.data_dir.is_none() {
        config.data_dir.clone_from(&env.data_dir);
    }
    let mut session = Session {
        p,
        env,
        opts: options,
        config,
        ollama_available: false,
        models: Vec::new(),
        managed: false,
        provisioning: None,
        validations: Vec::new(),
        rebuild: None,
        system1_preset_failed: false,
    };
    match session.walk().await {
        Ok(warnings) => Ok(WizardOutcome {
            config: session.config,
            saved: Some(env.config_path.clone()),
            cancelled: false,
            provisioning: session.provisioning,
            validations: session.validations,
            rebuild_test: session.rebuild,
            warnings,
        }),
        Err(Stop::Cancelled) => {
            session.report_cancel();
            Ok(WizardOutcome {
                config: session.config,
                saved: None,
                cancelled: true,
                provisioning: session.provisioning,
                validations: session.validations,
                rebuild_test: session.rebuild,
                warnings: Vec::new(),
            })
        }
        Err(Stop::Fail(err)) => Err(err),
    }
}

impl Session<'_> {
    pub(super) fn paths(&self) -> AnchorPaths {
        self.config.paths()
    }

    fn report_cancel(&self) {
        let path = self.env.config_path.display();
        if self.env.config_path.exists() {
            self.p
                .note(&format!("Setup cancelled. {path} was left untouched."));
        } else {
            self.p
                .note(&format!("Setup cancelled. Nothing was written to {path}."));
        }
        if let Some(report) = &self.provisioning
            && (report.installed || report.pulled || report.started)
        {
            self.p.note(&format!(
                "Already done before cancelling: Ollama {} at {} with model {}. Remove it with `anchor ollama teardown`.",
                if report.installed { "was installed" } else { "was started" },
                report.base_url,
                report.model
            ));
        }
    }

    async fn walk(&mut self) -> Step<Vec<crate::config::Issue>> {
        self.p.note("Anchor setup. Press Esc at any prompt to cancel; nothing is written until the final confirmation.");
        self.step_mode()?;
        self.step_ollama().await?;
        self.step_rebuild().await?;
        self.step_tiers().await?;
        self.step_sources().await?;
        self.step_embeddings().await?;
        self.step_review().await
    }

    // -- helpers ----------------------------------------------------------------------------

    /// Asks for text until `check` accepts it; `check` returns the normalised value.
    pub(super) fn ask_valid(
        &self,
        message: &str,
        default: Option<&str>,
        check: impl Fn(&str) -> std::result::Result<String, String>,
    ) -> Step<String> {
        loop {
            let value = self.p.text(message, default)?;
            match check(value.trim()) {
                Ok(value) => return Ok(value),
                Err(problem) => self.p.warn(&problem),
            }
        }
    }

    pub(super) fn ask_env_name(
        &self,
        message: &str,
        default: Option<&str>,
        optional: bool,
    ) -> Step<Option<String>> {
        let value = self.ask_valid(message, default, |text| {
            if text.is_empty() && optional {
                Ok(String::new())
            } else if is_env_name(text) {
                Ok(text.to_owned())
            } else {
                Err(format!(
                    "`{text}` is not an environment variable name (letters, digits and `_`). Enter the NAME of the variable, never the key itself."
                ))
            }
        })?;
        if value.is_empty() {
            return Ok(None);
        }
        if self.env.lookup(&value).is_none() {
            self.p.warn(&format!(
                "{value} is not set in this shell. Export it before starting Anchor; endpoint validation needs it now."
            ));
        }
        Ok(Some(value))
    }

    pub(super) fn progress_sink<'p>(
        p: &'p dyn Prompter,
    ) -> impl Fn(ProvisionEvent) + Send + Sync + 'p {
        move |event| match event {
            ProvisionEvent::Step(text) => p.progress(Progress::Step(text)),
            ProvisionEvent::Download {
                file,
                received,
                total,
            } => p.progress(Progress::Bytes {
                label: format!("download {file}"),
                done: received,
                total,
            }),
            ProvisionEvent::Extract { entries } => p.progress(Progress::Bytes {
                label: "extract".to_owned(),
                done: entries,
                total: None,
            }),
            ProvisionEvent::Pull {
                model,
                status,
                completed,
                total,
            } => match total {
                Some(total) => p.progress(Progress::Bytes {
                    label: format!("pull {model}"),
                    done: completed.unwrap_or(0),
                    total: Some(total),
                }),
                None => p.progress(Progress::Step(format!("{model}: {status}"))),
            },
        }
    }

    // -- 1. mode ----------------------------------------------------------------------------

    fn step_mode(&mut self) -> Step {
        let mode = match self.opts.mode {
            Some(mode) => mode,
            None => {
                let modes = [Mode::Standalone, Mode::Client, Mode::Server];
                let choices = [
                    Choice::with_help("Standalone", "everything runs on this machine"),
                    Choice::with_help(
                        "Client",
                        "also exchanges .contextpack deltas with a central server",
                    ),
                    Choice::with_help("Server", "the 24/7 central sync server"),
                ];
                let default = modes
                    .iter()
                    .position(|m| *m == self.config.mode)
                    .unwrap_or(0);
                modes[self
                    .p
                    .select("How will this installation operate?", &choices, default)?]
            }
        };
        self.config.mode = mode;
        match mode {
            Mode::Standalone => {}
            Mode::Client => self.client_settings()?,
            Mode::Server => {
                let current = self.config.sync.listen.clone();
                let listen = self.ask_valid(
                    "Address the sync server listens on (host:port; 0.0.0.0:7891 accepts other machines)",
                    Some(&current),
                    |text| match text.rsplit_once(':') {
                        Some((host, port))
                            if !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0) =>
                        {
                            Ok(text.to_owned())
                        }
                        _ => Err(format!(
                            "`{text}` is not a host:port address, e.g. 0.0.0.0:7891"
                        )),
                    },
                )?;
                self.config.sync.listen = listen;
            }
        }
        Ok(())
    }

    fn client_settings(&mut self) -> Step {
        let transports = [SyncTransport::WebSocket, SyncTransport::Grpc];
        let choices = [
            Choice::with_help("WebSocket", "ws:// or wss:// URL"),
            Choice::with_help("gRPC", "http:// or https:// URL"),
        ];
        let default = transports
            .iter()
            .position(|t| *t == self.config.sync.transport)
            .unwrap_or(0);
        let transport =
            transports[self
                .p
                .select("Transport to the sync server", &choices, default)?];
        self.config.sync.transport = transport;
        let schemes: &[&str] = match transport {
            SyncTransport::WebSocket => &["ws", "wss"],
            SyncTransport::Grpc => &["http", "https"],
        };
        let current = self.config.sync.server_url.clone();
        let url = self.ask_valid("Sync server URL", current.as_deref(), |text| {
            let scheme = text
                .split_once("://")
                .map(|(scheme, rest)| (scheme.to_ascii_lowercase(), rest));
            match scheme {
                Some((scheme, rest)) if schemes.contains(&scheme.as_str()) && !rest.is_empty() => {
                    Ok(text.to_owned())
                }
                _ => Err(format!(
                    "`{text}` does not fit the transport: use {}",
                    schemes
                        .iter()
                        .map(|s| format!("{s}://"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                )),
            }
        })?;
        self.config.sync.server_url = Some(url);
        let token_default = self
            .config
            .sync
            .auth_token_env
            .clone()
            .unwrap_or_else(|| "ANCHOR_SYNC_TOKEN".to_owned());
        self.config.sync.auth_token_env = self.ask_env_name(
            "Environment variable holding the server's bearer token (name only; empty for none)",
            Some(&token_default),
            true,
        )?;
        Ok(())
    }

    // -- 2. ollama --------------------------------------------------------------------------

    fn no_local_model(&mut self) {
        self.ollama_available = false;
        self.managed = false;
        self.models.clear();
        self.config.ollama.managed = false;
        self.config.ollama.auto_provision = false;
        self.p.warn(
            "Continuing without a local model: System 1 will use the built-in heuristic classifier, which is less accurate for intent parsing and relevance filtering. Run `anchor setup` or `anchor ollama provision` later to add one.",
        );
    }

    async fn adopt_server(&mut self, models: Vec<String>) {
        self.ollama_available = true;
        self.models = models;
        let paths = self.paths();
        self.managed = self
            .env
            .ollama
            .is_managed(&self.config.ollama, &paths)
            .await;
        self.config.ollama.managed = self.managed;
    }

    fn ask_ollama_url(&mut self) -> Step {
        let current = self.config.ollama.base_url.clone();
        let url = self.ask_valid(
            "Ollama base URL (e.g. http://gpu-box.lan:11434)",
            Some(&current),
            |text| normalize_base_url(text).map_err(|err| err.to_string()),
        )?;
        self.config.ollama.base_url = url;
        self.config.ollama.managed = false;
        Ok(())
    }

    async fn step_ollama(&mut self) -> Step {
        loop {
            let url = self.config.ollama.base_url.clone();
            let status = self.env.ollama.probe(&url).await;
            if status.reachable() {
                self.p.note(&format!(
                    "Found Ollama {} at {url} with {} model(s).",
                    status.version.as_deref().unwrap_or("?"),
                    status.models.len()
                ));
                if self
                    .p
                    .confirm(&format!("Use the Ollama server at {url}?"), true)?
                {
                    self.adopt_server(status.models).await;
                    return Ok(());
                }
                self.ask_ollama_url()?;
                continue;
            }
            if self.opts.skip_provisioning {
                self.no_local_model();
                return Ok(());
            }
            self.p.note(&format!(
                "No Ollama server answers at {url}. Anchor can install Ollama under its own data directory (no root needed), start it, pull {} and verify it. Nothing outside that directory is touched.",
                self.config.ollama.default_model
            ));
            let choices = [
                Choice::with_help(
                    "Install and manage Ollama locally",
                    "recommended; downloads Ollama and a small model",
                ),
                Choice::with_help("Enter a different Ollama URL", "e.g. a GPU box on the LAN"),
                Choice::with_help(
                    "Continue without a local model",
                    "System 1 falls back to a heuristic classifier",
                ),
            ];
            let default = if self.config.ollama.auto_provision {
                0
            } else {
                2
            };
            match self.p.select("What should happen?", &choices, default)? {
                0 => match self.provision().await {
                    Ok(()) => return Ok(()),
                    Err(err) if self.opts.yes => return Err(Stop::Fail(err)),
                    Err(err) => self.p.warn(&format!("Provisioning failed: {err}")),
                },
                1 => self.ask_ollama_url()?,
                _ => {
                    self.no_local_model();
                    return Ok(());
                }
            }
        }
    }

    async fn provision(&mut self) -> Result<()> {
        // A managed installation lives on this machine: never point it at a remote URL.
        let local = crate::engine::router::endpoint_locality(&self.config.ollama.base_url);
        if local != crate::base::Locality::Local {
            self.config.ollama.base_url = crate::ollama::DEFAULT_BASE_URL.to_owned();
        }
        self.config.ollama.auto_provision = true;
        let paths = self.paths();
        paths.ensure()?;
        let sink = Self::progress_sink(self.p);
        let result = self
            .env
            .ollama
            .provision(&self.config.ollama, &paths, &sink)
            .await;
        self.p.progress(Progress::Done);
        let report = result?;
        self.p.note(&format!(
            "Ollama {} ready at {} with model {} ({} ms).",
            report.version.as_deref().unwrap_or("?"),
            report.base_url,
            report.model,
            report.total_ms
        ));
        if let Some(verification) = &report.verification {
            self.p.note(&format!(
                "Verified: {} answered in {} ms.",
                verification.model, verification.latency_ms
            ));
        }
        self.config.ollama.base_url.clone_from(&report.base_url);
        self.config.ollama.managed = report.mode != ServerMode::External;
        self.managed = self.config.ollama.managed;
        self.ollama_available = true;
        self.models = self.env.ollama.probe(&report.base_url).await.models;
        self.provisioning = Some(report);
        Ok(())
    }

    // -- 3. rebuild test --------------------------------------------------------------------

    async fn step_rebuild(&mut self) -> Step {
        if !self.opts.rebuild_test {
            if !(self.ollama_available && self.managed) {
                return Ok(());
            }
            if !self.p.confirm(
                "Run the bootstrap resilience test now? It wipes the managed Ollama installation and models, rebuilds them and verifies the result.",
                false,
            )? {
                return Ok(());
            }
        }
        if !(self.ollama_available && self.managed) {
            let reason = if self.ollama_available {
                format!(
                    "The Ollama server at {} was not installed by Anchor, so it is never wiped or rebuilt. Only a managed installation (set up through this wizard or `anchor ollama provision`) can be tested.",
                    self.config.ollama.base_url
                )
            } else {
                "There is no Ollama installation to test: none is reachable and Anchor did not install one.".to_owned()
            };
            self.p
                .warn(&format!("Bootstrap resilience test refused. {reason}"));
            self.rebuild = Some(RebuildTest {
                refused: Some(reason),
                ..RebuildTest::default()
            });
            return Ok(());
        }
        self.p
            .note("Bootstrap resilience test: wiping and rebuilding the managed Ollama backend...");
        let paths = self.paths();
        let sink = Self::progress_sink(self.p);
        let result = self
            .env
            .ollama
            .rebuild(&self.config.ollama, &paths, &sink)
            .await;
        self.p.progress(Progress::Done);
        match result {
            Ok(report) => {
                self.p.note(&format!(
                    "Rebuild finished in {} ms: wipe {} ms ({} removed, {} bytes freed), provision {} ms, verify {} ms.",
                    report.total_ms,
                    report.teardown_ms,
                    report.teardown.removed.len(),
                    report.teardown.bytes_freed,
                    report.provision_ms,
                    report.verify_ms
                ));
                for step in &report.provision.steps {
                    self.p.note(&format!("  {:<10} {} ms", step.step, step.ms));
                }
                self.p.note(&format!(
                    "Verification: {} answered in {} ms ({}).",
                    report.verification.model,
                    report.verification.latency_ms,
                    report.verification.response.trim()
                ));
                if report.provision.base_url != self.config.ollama.base_url {
                    self.p.note(&format!(
                        "The rebuilt server listens at {} now; the configuration follows.",
                        report.provision.base_url
                    ));
                    let old = self.config.ollama.base_url.clone();
                    self.config
                        .ollama
                        .base_url
                        .clone_from(&report.provision.base_url);
                    for tier in [
                        &mut self.config.models.system1,
                        &mut self.config.models.system2,
                    ] {
                        if tier.provider == ProviderKind::Ollama
                            && tier.base_url.as_deref() == Some(old.as_str())
                        {
                            tier.base_url = Some(report.provision.base_url.clone());
                        }
                    }
                }
                self.models = self
                    .env
                    .ollama
                    .probe(&report.provision.base_url)
                    .await
                    .models;
                self.rebuild = Some(RebuildTest {
                    report: Some(report),
                    ..RebuildTest::default()
                });
            }
            Err(err) => {
                self.p.warn(&format!("The rebuild test failed: {err}"));
                self.rebuild = Some(RebuildTest {
                    error: Some(err.to_string()),
                    ..RebuildTest::default()
                });
            }
        }
        Ok(())
    }

    // -- 4 & 5. tiers and validation gating ------------------------------------------------

    pub(super) fn tier_mut(&mut self, tier: ModelTier) -> &mut TierConfig {
        match tier {
            ModelTier::System1 => &mut self.config.models.system1,
            ModelTier::System2 => &mut self.config.models.system2,
        }
    }

    async fn step_tiers(&mut self) -> Step {
        self.configure_system1().await?;
        self.gate(ModelTier::System1).await?;
        self.configure_system2()?;
        self.gate(ModelTier::System2).await?;
        let system2 = &self.config.models.system2;
        if system2.is_enabled()
            && matches!(
                system2.provider,
                ProviderKind::Anthropic | ProviderKind::OpenAiCompatible
            )
        {
            let allow = self.config.models.routing.allow_cloud;
            let allow = self.p.confirm(
                "Allow queries to escalate to the cloud model (System 2)?",
                allow,
            )?;
            self.config.models.routing.allow_cloud = allow;
            if allow {
                let current = self
                    .config
                    .models
                    .routing
                    .max_cloud_usd_per_day
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let budget = self.ask_valid(
                    "Daily cloud budget in USD (empty for no limit)",
                    Some(&current),
                    |text| match text {
                        "" => Ok(String::new()),
                        _ => match text.parse::<f64>() {
                            Ok(v) if v.is_finite() && v >= 0.0 => Ok(text.to_owned()),
                            _ => Err(format!("`{text}` is not a non-negative amount")),
                        },
                    },
                )?;
                self.config.models.routing.max_cloud_usd_per_day = budget.parse::<f64>().ok();
            }
        }
        Ok(())
    }

    pub(super) async fn refresh_models(&mut self) {
        self.models = self
            .env
            .ollama
            .probe(&self.config.ollama.base_url)
            .await
            .models;
    }

    pub(super) async fn pull(&mut self, model: &str) -> bool {
        let sink = Self::progress_sink(self.p);
        let result = self
            .env
            .ollama
            .pull_model(&self.config.ollama.base_url, model, &sink)
            .await;
        self.p.progress(Progress::Done);
        match result {
            Ok(()) => {
                self.refresh_models().await;
                true
            }
            Err(err) => {
                self.p.warn(&format!("Could not pull {model}: {err}"));
                false
            }
        }
    }

    pub(super) async fn configure_system1(&mut self) -> Step {
        if !self.ollama_available {
            self.config.models.system1.provider = ProviderKind::Disabled;
            return Ok(());
        }
        let current = self.config.models.system1.clone();
        let model = loop {
            let preset = if self.system1_preset_failed {
                None
            } else {
                self.opts.system1_model.clone()
            };
            if let Some(model) = preset {
                if !self.models.iter().any(|m| m == &model) {
                    self.p
                        .note(&format!("{model} is not on the server yet; pulling it."));
                    if !self.pull(&model).await {
                        if self.opts.yes {
                            return Err(Stop::Fail(Error::config(format!("cannot pull {model}"))));
                        }
                        self.p.warn("Choose another model.");
                        self.system1_preset_failed = true;
                        continue;
                    }
                }
                break Some(model);
            }
            let mut choices: Vec<Choice> = self.models.iter().map(Choice::new).collect();
            let pull_index = choices.len();
            choices.push(Choice::with_help(
                "Pull another model",
                "download it onto the server",
            ));
            choices.push(Choice::with_help(
                "Heuristic only",
                "no local model; less accurate",
            ));
            let preferred = [
                current.model.as_str(),
                self.config.ollama.default_model.as_str(),
            ];
            let default = preferred
                .iter()
                .find_map(|want| self.models.iter().position(|m| m == want))
                .unwrap_or(if self.models.is_empty() {
                    pull_index
                } else {
                    0
                });
            let picked = self.p.select(
                "Model for System 1 (fast local classifier)",
                &choices,
                default,
            )?;
            if picked == pull_index {
                let default = self.config.ollama.default_model.clone();
                let name = self.ask_valid("Model to pull", Some(&default), |text| {
                    if text.is_empty() {
                        Err("enter a model name such as llama3.2:3b".to_owned())
                    } else {
                        Ok(text.to_owned())
                    }
                })?;
                if self.pull(&name).await {
                    break Some(name);
                }
            } else if picked == pull_index + 1 {
                break None;
            } else {
                break Some(self.models[picked].clone());
            }
        };
        let base_url = self.config.ollama.base_url.clone();
        let tier = &mut self.config.models.system1;
        match model {
            Some(model) => {
                tier.provider = ProviderKind::Ollama;
                tier.model = model;
                tier.base_url = Some(base_url);
                tier.api_key_env = None;
            }
            None => {
                tier.provider = ProviderKind::Disabled;
                self.p.warn("System 1 is heuristic-only: classification and filtering will not use a model.");
            }
        }
        Ok(())
    }
}
