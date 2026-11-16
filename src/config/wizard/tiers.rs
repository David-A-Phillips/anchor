//! System 2 configuration and the endpoint validation gate.

use crate::base::ModelTier;
use crate::engine::router::{OPENAI_BASE_URL, ProviderKind, TierConfig};

use super::Choice;
use super::flow::{Session, Step, Stop};
use super::validator::{TierValidation, provider_name};

/// The model suggested for Anthropic as System 2.
const SYSTEM2_ANTHROPIC_MODEL: &str = "claude-opus-5-5";

fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::System1 => "System 1",
        ModelTier::System2 => "System 2",
    }
}

impl Session<'_> {
    fn required_text(&self, message: &str, default: Option<&str>) -> Step<String> {
        self.ask_valid(message, default, |text| {
            if text.is_empty() {
                Err("this value is required".to_owned())
            } else {
                Ok(text.to_owned())
            }
        })
    }

    fn http_url(&self, message: &str, default: Option<&str>) -> Step<String> {
        self.ask_valid(message, default, |text| {
            let rest = text
                .strip_prefix("http://")
                .or_else(|| text.strip_prefix("https://"));
            match rest {
                Some(rest) if !rest.is_empty() => Ok(text.trim_end_matches('/').to_owned()),
                _ => Err(format!("`{text}` is not an http(s) URL")),
            }
        })
    }

    pub(super) fn configure_system2(&mut self) -> Step {
        if let Some(preset) = self.opts.system2.clone() {
            self.config.models.system2 = preset;
            return Ok(());
        }
        let current = self.config.models.system2.clone();
        let providers = [
            ProviderKind::Anthropic,
            ProviderKind::OpenAiCompatible,
            ProviderKind::Ollama,
            ProviderKind::Disabled,
        ];
        let choices = [
            Choice::with_help("Anthropic", "Claude through the Anthropic API"),
            Choice::with_help(
                "OpenAI-compatible endpoint",
                "OpenAI, vLLM, LM Studio, a gateway...",
            ),
            Choice::with_help(
                "Large model on an Ollama server",
                "stays local; needs a big GPU",
            ),
            Choice::with_help("Disabled", "answers come from System 1 and retrieval only"),
        ];
        let default = providers
            .iter()
            .position(|p| *p == current.provider)
            .unwrap_or(3);
        let provider = providers[self.p.select(
            "Engine for System 2 (heavy reasoning and synthesis)",
            &choices,
            default,
        )?];
        let same = current.provider == provider;
        let mut tier = TierConfig {
            provider,
            ..current.clone()
        };
        match provider {
            ProviderKind::Anthropic => {
                let model_default = if same && !current.model.is_empty() {
                    current.model.clone()
                } else {
                    SYSTEM2_ANTHROPIC_MODEL.to_owned()
                };
                tier.model = self.required_text("Anthropic model", Some(&model_default))?;
                let key_default = current
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| "ANTHROPIC_API_KEY".to_owned());
                tier.api_key_env = self.ask_env_name(
                    "Environment variable holding the Anthropic API key (name only)",
                    Some(&key_default),
                    false,
                )?;
                if !same {
                    tier.base_url = None;
                }
            }
            ProviderKind::OpenAiCompatible => {
                let url_default = match (&current.base_url, same) {
                    (Some(url), true) => url.clone(),
                    _ => OPENAI_BASE_URL.to_owned(),
                };
                tier.base_url = Some(self.http_url("Endpoint base URL", Some(&url_default))?);
                let model_default = if same { current.model.as_str() } else { "" };
                tier.model = self
                    .required_text("Model name", Some(model_default).filter(|m| !m.is_empty()))?;
                tier.api_key_env = self.ask_env_name(
                    "Environment variable holding the API key (name only; empty if the endpoint needs none)",
                    Some(current.api_key_env.as_deref().unwrap_or("")),
                    true,
                )?;
            }
            ProviderKind::Ollama => {
                let url_default = match (&current.base_url, same) {
                    (Some(url), true) => url.clone(),
                    _ => self.config.ollama.base_url.clone(),
                };
                let url = self.http_url("Ollama server for System 2", Some(&url_default))?;
                let on_known_server =
                    url.trim_end_matches('/') == self.config.ollama.base_url.trim_end_matches('/');
                let model_default = if same {
                    current.model.clone()
                } else {
                    String::new()
                };
                let model = if on_known_server && !self.models.is_empty() {
                    let mut options: Vec<Choice> = self.models.iter().map(Choice::new).collect();
                    options.push(Choice::new("Other (type a name)"));
                    let preselect = self
                        .models
                        .iter()
                        .position(|m| *m == model_default)
                        .unwrap_or(0);
                    let picked =
                        self.p
                            .select("Large model on that server", &options, preselect)?;
                    if picked < self.models.len() {
                        self.models[picked].clone()
                    } else {
                        self.required_text("Model name", None)?
                    }
                } else {
                    self.required_text(
                        "Model name",
                        Some(model_default.as_str()).filter(|m| !m.is_empty()),
                    )?
                };
                tier.base_url = Some(url);
                tier.model = model;
                tier.api_key_env = None;
            }
            ProviderKind::Disabled => {}
        }
        self.config.models.system2 = tier;
        Ok(())
    }

    /// Validates the tier with a real completion until it passes, is replaced or is disabled. The
    /// configuration is never written while an enabled tier is failing.
    pub(super) async fn gate(&mut self, tier: ModelTier) -> Step {
        loop {
            let config = self.tier_mut(tier).clone();
            self.validations.retain(|v| v.tier != tier);
            if !config.is_enabled() {
                return Ok(());
            }
            let result = self.check_tier(tier, &config).await;
            let line = format!(
                "{} ({}): {}",
                tier_label(tier),
                provider_name(config.provider),
                result.message
            );
            if result.ok {
                self.p.note(&format!("Validated {line}"));
                self.validations.push(result);
                return Ok(());
            }
            self.p.warn(&format!("Validation failed for {line}"));
            if self.opts.yes {
                return Err(Stop::Fail(crate::base::Error::config(format!(
                    "{} failed endpoint validation: {}",
                    tier_label(tier),
                    result.message
                ))));
            }
            let disable_label = match tier {
                ModelTier::System1 => "Use the heuristic classifier only (disable the model)",
                ModelTier::System2 => "Disable System 2",
            };
            let choices = [
                Choice::with_help(
                    "Retry",
                    "after fixing the environment or starting the server",
                ),
                Choice::with_help(
                    "Change this tier's settings",
                    "pick another model or endpoint",
                ),
                Choice::new(disable_label),
            ];
            match self.p.select(
                &format!("{} did not pass validation; the configuration cannot be saved with a failing tier", tier_label(tier)),
                &choices,
                1,
            )? {
                0 => {}
                1 => match tier {
                    ModelTier::System1 => self.configure_system1().await?,
                    ModelTier::System2 => self.configure_system2()?,
                },
                _ => {
                    self.tier_mut(tier).provider = ProviderKind::Disabled;
                    if tier == ModelTier::System1 {
                        self.p.warn("System 1 is heuristic-only: classification and filtering will not use a model.");
                    }
                }
            }
        }
    }

    async fn check_tier(&self, tier: ModelTier, config: &TierConfig) -> TierValidation {
        if let Some(name) = &config.api_key_env
            && self.env.lookup(name).is_none()
        {
            return TierValidation::failed(
                tier,
                config,
                format!(
                    "environment variable {name} is not set; export it in the shell that runs Anchor, then retry"
                ),
            );
        }
        self.env
            .validator
            .validate(tier, config, &self.env.env)
            .await
    }
}
