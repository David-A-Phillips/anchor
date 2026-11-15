//! Hermetic tests of the wizard: scripted prompts, the provisioning engine's own fakes, scripted
//! validators and models.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use super::*;
use crate::base::{ModelError, ModelTier, SourceKind, TrustLevel};
use crate::config::AnchorPaths;
use crate::config::provision::testing::{FakeOllama, FixedLocator, TestRig};
use crate::config::provision::{OllamaManager, ServerMode};
use crate::engine::router::{ProviderKind, ScriptedModel};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

/// Passes or fails according to a script (pass when the script is empty) and records its calls.
#[derive(Default)]
struct FakeValidator {
    script: Mutex<VecDeque<bool>>,
    calls: Mutex<Vec<ModelTier>>,
}

#[async_trait]
impl EndpointValidator for FakeValidator {
    async fn validate(&self, tier: ModelTier, config: &TierConfig, _env: &EnvFn) -> TierValidation {
        self.calls.lock().push(tier);
        if self.script.lock().pop_front().unwrap_or(true) {
            TierValidation::passed(
                tier,
                config,
                std::time::Duration::from_millis(12),
                "answered".to_owned(),
            )
        } else {
            TierValidation::failed(tier, config, "scripted failure")
        }
    }
}

#[derive(Default)]
struct FakeFetcher {
    fetched: Mutex<Vec<String>>,
}

#[async_trait]
impl EmbeddingFetcher for FakeFetcher {
    async fn fetch(
        &self,
        models_dir: &Path,
        repo_id: &str,
        _progress: &(dyn Fn(crate::engine::embed::DownloadProgress) + Send + Sync),
    ) -> crate::Result<PathBuf> {
        self.fetched.lock().push(repo_id.to_owned());
        Ok(models_dir.join(repo_id.replace('/', "--")))
    }
}

struct Harness {
    rig: TestRig,
    validator: Arc<FakeValidator>,
    fetcher: Arc<FakeFetcher>,
    vars: Arc<Mutex<BTreeMap<String, String>>>,
}

impl Harness {
    async fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            rig: TestRig::new().await?,
            validator: Arc::new(FakeValidator::default()),
            fetcher: Arc::new(FakeFetcher::default()),
            vars: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    fn dir(&self) -> &Path {
        self.rig.dir.path()
    }

    fn config_path(&self) -> PathBuf {
        self.dir().join("anchor.toml")
    }

    fn script_validator(&self, results: &[bool]) {
        self.validator.script.lock().extend(results.iter().copied());
    }

    fn set_var(&self, name: &str, value: &str) {
        self.vars.lock().insert(name.to_owned(), value.to_owned());
    }

    /// Defaults, pointed at the rig's (unreachable until provisioned) Ollama endpoint.
    fn base_config(&self) -> AnchorConfig {
        let mut config = AnchorConfig::default();
        config.data_dir = Some(self.dir().join("data"));
        config.ollama = self.rig.config.clone();
        config.models.system1.model = "tiny:latest".to_owned();
        config.models.system1.base_url = Some(self.rig.config.base_url.clone());
        config
    }

    fn config_for(&self, ollama_url: &str) -> AnchorConfig {
        let mut config = self.base_config();
        config.ollama.base_url = ollama_url.to_owned();
        config.models.system1.base_url = Some(ollama_url.to_owned());
        config
    }

    fn env(&self, existing: Option<AnchorConfig>) -> WizardEnvironment {
        let launcher = self.rig.launcher.clone();
        let home = self.dir().join("home");
        let backend = ManagerBackend::with_factory(
            move |config, paths| {
                OllamaManager::with_parts(
                    config.clone(),
                    paths.ollama_dir(),
                    launcher.clone(),
                    Arc::new(FixedLocator(None)),
                )
                .with_home(Some(home.clone()))
            },
            self.rig.options.clone(),
        );
        let vars = self.vars.clone();
        WizardEnvironment {
            config_path: self.config_path(),
            existing,
            data_dir: Some(self.dir().join("data")),
            env: Arc::new(move |name| vars.lock().get(name).cloned()),
            ollama: Arc::new(backend),
            validator: self.validator.clone(),
            embeddings: self.fetcher.clone(),
        }
    }

    async fn run(
        &self,
        existing: Option<AnchorConfig>,
        answers: Vec<Answer>,
        options: &WizardOptions,
    ) -> (ScriptedPrompter, crate::Result<WizardOutcome>) {
        let prompter = ScriptedPrompter::new(answers);
        let result = run_wizard(&prompter, &self.env(existing), options).await;
        (prompter, result)
    }
}

fn load(path: &Path) -> std::result::Result<AnchorConfig, Box<dyn std::error::Error>> {
    Ok(AnchorConfig::load(path)?)
}

/// Answers for everything after the Ollama step: System 1 default, System 2 disabled, no sources,
/// hash embeddings, save.
fn tail() -> Vec<Answer> {
    vec![
        Answer::Default,
        Answer::pick("Disabled"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ]
}

fn concat(parts: Vec<Vec<Answer>>) -> Vec<Answer> {
    parts.into_iter().flatten().collect()
}

fn add_source(
    kind: &str,
    location: &Path,
    name: &str,
    trust: Answer,
    extra: Vec<Answer>,
) -> Vec<Answer> {
    let mut answers = vec![
        Answer::pick("Add a source"),
        Answer::pick(kind),
        Answer::text(location.to_string_lossy()),
        Answer::text(name),
        trust,
    ];
    answers.extend(extra);
    answers
}

#[tokio::test]
async fn happy_path_with_a_running_ollama() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest", "other:7b"], None).await?;
    let repo = h.dir().join("payments");
    std::fs::create_dir_all(&repo)?;
    let answers = concat(vec![
        vec![Answer::Default, Answer::Default],
        vec![Answer::Default, Answer::pick("Disabled")],
        add_source(
            "Repository",
            &repo,
            "Payments",
            Answer::pick("SourceOfTruth"),
            vec![],
        ),
        vec![
            Answer::pick("Done with sources"),
            Answer::pick("Hash"),
            Answer::Default,
        ],
    ]);
    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;

    assert!(!outcome.cancelled);
    assert_eq!(outcome.saved.as_deref(), Some(h.config_path().as_path()));
    assert_eq!(prompter.remaining(), 0);
    assert!(
        outcome.provisioning.is_none(),
        "an external server is only used"
    );
    assert!(!outcome.config.ollama.managed);
    assert_eq!(outcome.validations.len(), 1);
    assert_eq!(outcome.validations[0].tier, ModelTier::System1);

    // The saved file loads back and validates cleanly.
    let loaded = AnchorConfig::load_with_report(&h.config_path())?;
    assert!(loaded.unknown_keys.is_empty());
    let report = loaded.report();
    assert!(report.is_ok() && report.warnings.is_empty(), "{report:?}");
    assert_eq!(loaded.config.models.system1.model, "tiny:latest");
    assert_eq!(loaded.config.sources.len(), 1);
    assert_eq!(loaded.config.sources[0].id.as_str(), "payments");
    assert!(
        h.dir().join("data").join("keys").is_dir(),
        "data layout created"
    );
    // Nothing was installed or pulled.
    assert!(fake.state().pulls.lock().expect("lock").is_empty());
    Ok(())
}

#[tokio::test]
async fn missing_ollama_is_provisioned_and_recorded_as_managed() -> TestResult {
    let h = Harness::new().await?;
    let answers = concat(vec![
        vec![
            Answer::Default,        // mode
            Answer::Default,        // what should happen: install (auto_provision is on)
            Answer::Confirm(false), // rebuild test offer
        ],
        tail(),
    ]);
    let (prompter, outcome) = h
        .run(Some(h.base_config()), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;

    let report = outcome.provisioning.as_ref().expect("provisioned");
    assert!(report.installed && report.started && report.pulled);
    assert_ne!(report.mode, ServerMode::External);
    assert!(outcome.config.ollama.managed);
    assert!(outcome.config.ollama.auto_provision);
    assert_eq!(outcome.config.models.system1.model, "tiny:latest");
    assert!(
        prompter
            .progress_events()
            .iter()
            .any(|p| matches!(p, Progress::Step(_) | Progress::Bytes { .. })),
        "live progress was reported"
    );
    let saved = load(&h.config_path())?;
    assert!(saved.ollama.managed);
    assert!(outcome.rebuild_test.is_none());
    Ok(())
}

#[tokio::test]
async fn declining_provisioning_leaves_heuristic_only_system1() -> TestResult {
    let h = Harness::new().await?;
    let answers = vec![
        Answer::Default,
        Answer::pick("Continue without a local model"),
        Answer::pick("Disabled"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ];
    let (prompter, outcome) = h
        .run(Some(h.base_config()), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;
    assert!(prompter.shown("heuristic classifier"));
    assert!(!outcome.config.models.system1.is_enabled());
    assert!(!outcome.config.ollama.auto_provision);
    assert!(outcome.provisioning.is_none());
    assert!(outcome.validations.is_empty());
    assert!(h.rig.launcher.specs().is_empty(), "nothing was started");
    let saved = load(&h.config_path())?;
    assert_eq!(saved.models.system1.provider, ProviderKind::Disabled);
    assert!(saved.validate().is_ok());
    Ok(())
}

#[tokio::test]
async fn unset_api_key_variable_is_caught_and_the_tier_can_be_disabled() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default, // system 1
        Answer::pick("Anthropic"),
        Answer::Default, // model claude-opus-5-5
        Answer::Default, // ANTHROPIC_API_KEY
        Answer::pick("Change this tier"),
        Answer::pick("Disabled"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ];
    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    assert!(prompter.shown("ANTHROPIC_API_KEY is not set"));
    assert!(prompter.shown("Validation failed for System 2"));
    assert!(!outcome.config.models.system2.is_enabled());
    // The endpoint validator was never asked about a key-less Anthropic tier.
    assert_eq!(*h.validator.calls.lock(), vec![ModelTier::System1]);
    let asked = prompter.asked();
    assert!(
        asked
            .iter()
            .any(|a| a.message.contains("cannot be saved with a failing tier"))
    );
    Ok(())
}

#[tokio::test]
async fn failing_endpoint_blocks_saving_until_it_passes() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    h.script_validator(&[true, false, true]);
    h.set_var("LLM_KEY", "secret");
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("OpenAI-compatible"),
        Answer::text("http://127.0.0.1:9/v1"),
        Answer::text("big-model"),
        Answer::text("LLM_KEY"),
        Answer::pick("Retry"),
        Answer::Confirm(true),
        Answer::text("5"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ];
    let (_, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    assert_eq!(
        *h.validator.calls.lock(),
        vec![ModelTier::System1, ModelTier::System2, ModelTier::System2]
    );
    assert_eq!(outcome.validations.len(), 2);
    assert!(outcome.validations.iter().all(|v| v.ok));
    let saved = load(&h.config_path())?;
    assert_eq!(
        saved.models.system2.provider,
        ProviderKind::OpenAiCompatible
    );
    assert_eq!(saved.models.system2.api_key_env.as_deref(), Some("LLM_KEY"));
    assert_eq!(saved.models.routing.max_cloud_usd_per_day, Some(5.0));
    Ok(())
}

#[tokio::test]
async fn cancelling_at_a_failing_tier_writes_nothing() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    h.script_validator(&[false]);
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::Cancel,
    ];
    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    assert!(outcome.cancelled && outcome.saved.is_none());
    assert!(!h.config_path().exists());
    assert!(prompter.shown("Nothing was written"));
    Ok(())
}

#[tokio::test]
async fn non_interactive_validation_failure_is_an_error_and_writes_nothing() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    h.script_validator(&[false]);
    let options = WizardOptions {
        yes: true,
        ..WizardOptions::default()
    };
    let (_, outcome) = h
        .run(Some(h.config_for(&fake.base_url())), vec![], &options)
        .await;
    let err = outcome.expect_err("failing tier");
    assert!(
        err.to_string().contains("failed endpoint validation"),
        "{err}"
    );
    assert!(!h.config_path().exists());
    Ok(())
}

#[tokio::test]
async fn each_kind_preselects_its_default_trust_and_the_explicit_choice_is_saved() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let kinds = [
        ("Repository", TrustLevel::SourceOfTruth),
        ("Wiki snapshot", TrustLevel::ReferenceOnly),
        ("API approval portal", TrustLevel::SourceOfTruth),
        ("Jira export", TrustLevel::ReferenceOnly),
        ("Org / portfolio model", TrustLevel::SourceOfTruth),
        ("Code-review export", TrustLevel::ReferenceOnly),
    ];
    let mut answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
    ];
    for (index, (kind, _)) in kinds.iter().enumerate() {
        let dir = h.dir().join(format!("src{index}"));
        std::fs::create_dir_all(&dir)?;
        // Wiki sources ask two extra questions.
        let extra = if *kind == "Wiki snapshot" {
            vec![Answer::Default, Answer::Default]
        } else {
            vec![]
        };
        answers.extend(add_source(
            kind,
            &dir,
            &format!("source {index}"),
            Answer::Default,
            extra,
        ));
    }
    // Explicit choices that differ from the default: a reference repository, an authoritative wiki.
    let repo = h.dir().join("legacy");
    std::fs::create_dir_all(&repo)?;
    answers.extend(add_source(
        "Repository",
        &repo,
        "legacy",
        Answer::pick("ReferenceOnly"),
        vec![],
    ));
    let wiki = h.dir().join("handbook");
    std::fs::create_dir_all(&wiki)?;
    answers.extend(add_source(
        "Wiki snapshot",
        &wiki,
        "handbook",
        Answer::pick("SourceOfTruth"),
        vec![Answer::Default, Answer::Default],
    ));
    answers.extend([
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ]);

    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    outcome?;

    let trust_prompts: Vec<_> = prompter
        .asked()
        .into_iter()
        .filter(|a| a.message.starts_with("Trust level for"))
        .collect();
    assert_eq!(trust_prompts.len(), 8);
    for ((kind, expected), asked) in kinds.iter().zip(&trust_prompts) {
        assert!(asked.message.contains(kind), "{}", asked.message);
        let want = if *expected == TrustLevel::SourceOfTruth {
            "SourceOfTruth"
        } else {
            "ReferenceOnly"
        };
        assert_eq!(asked.default_label(), Some(want), "{kind}");
        assert!(
            asked.choices.iter().all(|c| c.help.is_some()),
            "each level is explained"
        );
    }

    let saved = load(&h.config_path())?;
    let trust = |id: &str| {
        saved
            .sources
            .iter()
            .find(|s| s.id.as_str() == id)
            .map(|s| s.trust)
    };
    assert_eq!(trust("source-0"), Some(TrustLevel::SourceOfTruth));
    assert_eq!(trust("source-1"), Some(TrustLevel::ReferenceOnly));
    assert_eq!(trust("legacy"), Some(TrustLevel::ReferenceOnly));
    assert_eq!(trust("handbook"), Some(TrustLevel::SourceOfTruth));
    let text = std::fs::read_to_string(h.config_path())?;
    assert!(text.contains("source_of_truth") && text.contains("reference_only"));
    Ok(())
}

#[tokio::test]
async fn wiki_comment_options_are_stored_in_the_source() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let wiki = h.dir().join("wiki");
    std::fs::create_dir_all(&wiki)?;
    let mut answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
    ];
    answers.extend(add_source(
        "Wiki snapshot",
        &wiki,
        "Eng Wiki",
        Answer::Default,
        vec![Answer::Confirm(true), Answer::Confirm(false)],
    ));
    answers.extend([
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ]);
    let (_, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    outcome?;
    let saved = load(&h.config_path())?;
    let source = &saved.sources[0];
    assert_eq!(source.id.as_str(), "eng-wiki");
    assert_eq!(source.name.as_deref(), Some("Eng Wiki"));
    assert_eq!(
        source.options.get("scrape_comments"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        source.options.get("include_dismissed_threads"),
        Some(&serde_json::json!(false))
    );
    Ok(())
}

#[tokio::test]
async fn repositories_are_discovered_and_multi_selected() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let root = h.dir().join("code");
    for repo in ["alpha", "group/beta", "gamma"] {
        std::fs::create_dir_all(root.join(repo).join(".git"))?;
    }
    std::fs::create_dir_all(root.join("alpha/vendor/nested/.git"))?; // inside a repo: not reported
    std::fs::create_dir_all(root.join("node_modules/dep/.git"))?; // dependency: skipped
    std::fs::create_dir_all(root.join("plain/dir"))?;
    let found = discover_repositories(&root);
    assert_eq!(
        found,
        vec![
            root.join("alpha"),
            root.join("gamma"),
            root.join("group/beta")
        ]
    );

    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
        Answer::pick("Discover git repositories"),
        Answer::text(root.to_string_lossy()),
        Answer::picks(["alpha", "group/beta"]),
        Answer::pick("ReferenceOnly"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ];
    let (_, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    outcome?;
    let saved = load(&h.config_path())?;
    let ids: Vec<_> = saved
        .sources
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    assert_eq!(ids, ["alpha", "beta"]);
    assert!(
        saved
            .sources
            .iter()
            .all(|s| s.kind == SourceKind::Repo && s.trust == TrustLevel::ReferenceOnly)
    );
    Ok(())
}

#[test]
fn source_names_become_well_formed_ids() {
    assert_eq!(slugify("Payments API").as_deref(), Some("payments-api"));
    assert_eq!(
        slugify("  --Weird__Name!!  ").as_deref(),
        Some("weird__name")
    );
    assert_eq!(slugify("v1.2").as_deref(), Some("v1.2"));
    assert_eq!(slugify("Ünï cøde").as_deref(), Some("n-c-de"));
    assert_eq!(slugify("!!!"), None);
    assert_eq!(slugify(""), None);
    assert!(slugify(&"x".repeat(200)).is_some_and(|s| s.len() == 64));
}

#[tokio::test]
async fn duplicate_names_get_unique_ids_and_malformed_names_are_re_asked() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let dir = h.dir().join("docs");
    std::fs::create_dir_all(&dir)?;
    let mut answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
    ];
    answers.extend(add_source(
        "Jira export",
        &dir,
        "Docs",
        Answer::Default,
        vec![],
    ));
    answers.extend(add_source(
        "Jira export",
        &dir,
        "Docs",
        Answer::Default,
        vec![],
    ));
    // The malformed name is rejected; the next answer is accepted.
    answers.extend([
        Answer::pick("Add a source"),
        Answer::pick("Jira export"),
        Answer::text(dir.to_string_lossy()),
        Answer::text("***"),
        Answer::text("Docs"),
        Answer::Default,
    ]);
    answers.extend([
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ]);
    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    let ids: Vec<_> = outcome
        .config
        .sources
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(ids, ["docs", "docs-2", "docs-3"]);
    assert!(prompter.shown("cannot become a source id"));
    assert!(prompter.shown("`docs` is taken; using `docs-2`"));
    Ok(())
}

#[tokio::test]
async fn client_mode_validates_the_server_url_and_token_name() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    h.set_var("SYNC_TOKEN", "t");
    let answers = vec![
        Answer::pick("Client"),
        Answer::Default,                          // WebSocket
        Answer::text("https://sync.example.com"), // wrong scheme for WebSocket
        Answer::text("sync.example.com"),         // no scheme
        Answer::text("wss://sync.example.com/ws"),
        Answer::text("not a name"), // invalid variable name
        Answer::text("SYNC_TOKEN"),
        Answer::Default, // use Ollama
        Answer::Default,
        Answer::pick("Disabled"),
        Answer::pick("Done with sources"),
        Answer::pick("Hash"),
        Answer::Default,
    ];
    let (prompter, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    assert_eq!(
        prompter
            .warnings()
            .iter()
            .filter(|w| w.contains("does not fit"))
            .count(),
        2
    );
    assert!(prompter.shown("NAME of the variable"));
    let saved = load(&h.config_path())?;
    assert_eq!(saved.mode, crate::config::Mode::Client);
    assert_eq!(
        saved.sync.server_url.as_deref(),
        Some("wss://sync.example.com/ws")
    );
    assert_eq!(saved.sync.auth_token_env.as_deref(), Some("SYNC_TOKEN"));
    assert!(outcome.warnings.is_empty());
    Ok(())
}

#[tokio::test]
async fn rerun_prefills_from_the_existing_configuration_and_preserves_untouched_sections()
-> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let repo = h.dir().join("svc");
    std::fs::create_dir_all(&repo)?;
    let mut existing = h.config_for(&fake.base_url());
    existing.mode = crate::config::Mode::Server;
    existing.sync.listen = "0.0.0.0:9001".to_owned();
    existing.web.port = 8123;
    existing.parser.max_file_bytes = 12345;
    let mut source = SourceConfig::new("svc", SourceKind::Repo, repo.to_string_lossy());
    source.trust = TrustLevel::ReferenceOnly;
    source.include = vec!["src/**".to_owned()];
    source.name = Some("Service".to_owned());
    existing.sources.push(source);
    existing.models.routing.escalation_confidence = 0.7;
    existing.save(&h.config_path())?;

    let answers = vec![
        Answer::Default, // mode (pre-selected: server)
        Answer::Default, // listen address (pre-filled)
        Answer::Default, // use the Ollama server
        Answer::Default, // system 1
        Answer::Default, // system 2 (disabled)
        Answer::Default, // sources menu: done, because sources exist
        Answer::Default, // embeddings
        Answer::Default, // save
    ];
    let (prompter, outcome) = h
        .run(Some(existing.clone()), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;
    let asked = prompter.asked();
    assert_eq!(asked[0].default_label(), Some("Server"));
    assert_eq!(
        asked[1].default,
        PromptDefault::Text(Some("0.0.0.0:9001".to_owned()))
    );
    assert_eq!(outcome.config, existing);
    assert_eq!(load(&h.config_path())?, existing);
    Ok(())
}

#[tokio::test]
async fn sources_can_be_edited_re_trusted_and_removed_on_a_rerun() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let mut existing = h.config_for(&fake.base_url());
    for (id, kind) in [("keep", SourceKind::Repo), ("drop", SourceKind::Jira)] {
        existing
            .sources
            .push(SourceConfig::new(id, kind, h.dir().to_string_lossy()));
    }
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Edit or re-trust"),
        Answer::pick("keep"),
        Answer::pick("Change the trust level"),
        Answer::pick("ReferenceOnly"),
        Answer::pick("Remove a source"),
        Answer::pick("drop"),
        Answer::Confirm(true),
        Answer::pick("Done with sources"),
        Answer::Default,
        Answer::Default,
    ];
    let (_, outcome) = h
        .run(Some(existing), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;
    assert_eq!(outcome.config.sources.len(), 1);
    assert_eq!(outcome.config.sources[0].id.as_str(), "keep");
    assert_eq!(outcome.config.sources[0].trust, TrustLevel::ReferenceOnly);
    Ok(())
}

#[tokio::test]
async fn cancelling_leaves_the_existing_file_byte_identical() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let existing = h.config_for(&fake.base_url());
    existing.save(&h.config_path())?;
    let before = std::fs::read(h.config_path())?;
    let mut answers = tail();
    // Cancel at the very last prompt: everything was configured, nothing may be written.
    let last = answers.len() - 1;
    answers[last] = Answer::Cancel;
    answers.splice(0..0, [Answer::Default, Answer::Default]);
    let (prompter, outcome) = h
        .run(Some(existing), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;
    assert!(outcome.cancelled);
    assert_eq!(std::fs::read(h.config_path())?, before);
    assert!(prompter.shown("was left untouched"));
    Ok(())
}

#[tokio::test]
async fn cancelling_after_provisioning_tells_what_was_installed() -> TestResult {
    let h = Harness::new().await?;
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Confirm(false),
        Answer::Cancel,
    ];
    let (prompter, outcome) = h
        .run(Some(h.base_config()), answers, &WizardOptions::default())
        .await;
    let outcome = outcome?;
    assert!(outcome.cancelled);
    assert!(outcome.provisioning.is_some());
    assert!(prompter.shown("Already done before cancelling"));
    assert!(prompter.shown("anchor ollama teardown"));
    assert!(!h.config_path().exists());
    Ok(())
}

#[tokio::test]
async fn non_interactive_options_preselect_everything() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest", "big:70b"], None).await?;
    h.set_var("GATEWAY_KEY", "k");
    let repo = h.dir().join("repo");
    std::fs::create_dir_all(&repo)?;
    let mut source = SourceConfig::new("repo", SourceKind::Repo, repo.to_string_lossy());
    source.trust = TrustLevel::ReferenceOnly;
    let options = WizardOptions {
        yes: true,
        mode: Some(crate::config::Mode::Standalone),
        sources: vec![source],
        system1_model: Some("big:70b".to_owned()),
        system2: Some(TierConfig {
            provider: ProviderKind::OpenAiCompatible,
            model: "gw".to_owned(),
            base_url: Some("http://127.0.0.1:9/v1".to_owned()),
            api_key_env: Some("GATEWAY_KEY".to_owned()),
            ..TierConfig::default()
        }),
        ..WizardOptions::default()
    };
    let (prompter, outcome) = h
        .run(Some(h.config_for(&fake.base_url())), vec![], &options)
        .await;
    let outcome = outcome?;
    assert_eq!(
        prompter.asked().len(),
        0,
        "the scripted prompter is never asked in --yes mode"
    );
    assert_eq!(outcome.config.models.system1.model, "big:70b");
    assert_eq!(outcome.config.models.system2.model, "gw");
    assert_eq!(outcome.config.sources[0].trust, TrustLevel::ReferenceOnly);
    assert_eq!(outcome.validations.len(), 2);
    assert!(h.config_path().exists());
    Ok(())
}

#[tokio::test]
async fn yes_provisions_a_missing_ollama_without_asking() -> TestResult {
    let h = Harness::new().await?;
    let options = WizardOptions {
        yes: true,
        ..WizardOptions::default()
    };
    let (_, outcome) = h.run(Some(h.base_config()), vec![], &options).await;
    let outcome = outcome?;
    assert!(outcome.provisioning.is_some());
    assert!(outcome.config.ollama.managed);
    assert!(h.config_path().exists());
    Ok(())
}

#[tokio::test]
async fn skip_provisioning_never_touches_the_system() -> TestResult {
    let h = Harness::new().await?;
    let options = WizardOptions {
        yes: true,
        skip_provisioning: true,
        ..WizardOptions::default()
    };
    let (_, outcome) = h.run(Some(h.base_config()), vec![], &options).await;
    let outcome = outcome?;
    assert!(outcome.provisioning.is_none());
    assert!(!outcome.config.models.system1.is_enabled());
    assert!(h.rig.launcher.specs().is_empty());
    Ok(())
}

#[tokio::test]
async fn rebuild_test_runs_against_a_managed_installation() -> TestResult {
    let h = Harness::new().await?;
    let options = WizardOptions {
        yes: true,
        rebuild_test: true,
        ..WizardOptions::default()
    };
    let (prompter, outcome) = h.run(Some(h.base_config()), vec![], &options).await;
    let outcome = outcome?;
    let test = outcome.rebuild_test.expect("rebuild ran");
    assert!(test.refused.is_none() && test.error.is_none(), "{test:?}");
    let report = test.report.expect("report");
    assert!(report.teardown.root_removed || !report.teardown.removed.is_empty());
    assert!(!report.verification.response.is_empty() || report.verification.completion_tokens > 0);
    assert!(prompter.shown("Rebuild finished"));
    assert!(outcome.config.ollama.managed);
    // The rebuilt backend serves the model again.
    assert!(
        AnchorPaths::new(h.dir().join("data"))
            .ollama_dir()
            .join(".anchor-managed")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn rebuild_test_is_refused_for_an_external_server() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let options = WizardOptions {
        yes: true,
        rebuild_test: true,
        ..WizardOptions::default()
    };
    let (prompter, outcome) = h
        .run(Some(h.config_for(&fake.base_url())), vec![], &options)
        .await;
    let outcome = outcome?;
    let test = outcome.rebuild_test.expect("recorded");
    assert!(test.report.is_none());
    assert!(
        test.refused
            .as_deref()
            .is_some_and(|r| r.contains("not installed by Anchor"))
    );
    assert!(prompter.shown("refused"));
    assert!(
        fake.state().deletes.lock().expect("lock").is_empty(),
        "nothing was deleted from the external server"
    );
    assert!(fake.state().pulls.lock().expect("lock").is_empty());
    Ok(())
}

#[cfg(feature = "embed-candle")]
#[tokio::test]
async fn candle_embeddings_download_only_on_request() -> TestResult {
    let h = Harness::new().await?;
    let fake = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let mut answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
    ];
    answers.extend([
        Answer::pick("Done with sources"),
        Answer::pick("Candle"),
        Answer::Default,
        Answer::Confirm(true),
        Answer::Default,
    ]);
    let (_, outcome) = h
        .run(
            Some(h.config_for(&fake.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    let outcome = outcome?;
    assert_eq!(
        *h.fetcher.fetched.lock(),
        vec![crate::engine::embed::DEFAULT_CANDLE_MODEL]
    );
    assert_eq!(
        outcome.config.embeddings.backend,
        crate::engine::embed::EmbeddingBackend::Candle
    );

    // Declining downloads nothing.
    let h2 = Harness::new().await?;
    let fake2 = FakeOllama::start_with(0, &["tiny:latest"], None).await?;
    let answers = vec![
        Answer::Default,
        Answer::Default,
        Answer::Default,
        Answer::pick("Disabled"),
        Answer::pick("Done with sources"),
        Answer::pick("Candle"),
        Answer::Default,
        Answer::Confirm(false),
        Answer::Default,
    ];
    let (prompter, outcome) = h2
        .run(
            Some(h2.config_for(&fake2.base_url())),
            answers,
            &WizardOptions::default(),
        )
        .await;
    outcome?;
    assert!(h2.fetcher.fetched.lock().is_empty());
    assert!(prompter.shown("not downloaded yet"));
    Ok(())
}

#[tokio::test]
async fn the_real_validator_sends_a_tiny_completion_and_explains_failures() -> TestResult {
    let ok = Arc::new(ScriptedModel::new(
        ModelTier::System1,
        crate::base::Locality::Local,
    ));
    ok.push_text("ok");
    let bad = Arc::new(ScriptedModel::new(
        ModelTier::System2,
        crate::base::Locality::Cloud,
    ));
    bad.push_error(ModelError::Auth("bad key".to_owned()));
    let (ok_model, bad_model) = (ok.clone(), bad.clone());
    let validator = ModelValidator::with_factory(move |tier, _, _| {
        let model: Arc<dyn crate::base::LanguageModel> = match tier {
            ModelTier::System1 => ok_model.clone(),
            ModelTier::System2 => bad_model.clone(),
        };
        Ok(Some(model))
    });
    let env: EnvFn = Arc::new(|_| None);
    let config = TierConfig {
        provider: ProviderKind::Ollama,
        model: "m".to_owned(),
        ..TierConfig::default()
    };
    let passed = validator.validate(ModelTier::System1, &config, &env).await;
    assert!(passed.ok && passed.latency.is_some(), "{passed:?}");
    let request = &ok.requests()[0];
    assert_eq!(request.max_tokens, Some(16));

    let cloud = TierConfig {
        provider: ProviderKind::Anthropic,
        model: "m".to_owned(),
        api_key_env: Some("K".to_owned()),
        ..TierConfig::default()
    };
    let failed = validator.validate(ModelTier::System2, &cloud, &env).await;
    assert!(!failed.ok);
    assert!(failed.message.contains("authentication failed") && failed.message.contains('K'));

    // The router-backed validator reports a missing key variable instead of calling out.
    let real = ModelValidator::router();
    let missing = real.validate(ModelTier::System2, &cloud, &env).await;
    assert!(
        !missing.ok && missing.message.contains("not set"),
        "{missing:?}"
    );
    Ok(())
}

#[test]
fn summary_lists_sources_with_trust_and_tier_status() {
    let mut config = AnchorConfig::default();
    config
        .sources
        .push(SourceConfig::new("core", SourceKind::Repo, "/src/core"));
    let validation = TierValidation::passed(
        ModelTier::System1,
        &config.models.system1,
        std::time::Duration::from_millis(40),
        "ok".to_owned(),
    );
    let lines = summarize(&config, &[validation]).join("\n");
    assert!(
        lines.contains("Source core [repo] SourceOfTruth"),
        "{lines}"
    );
    assert!(lines.contains("validated, 40 ms"));
    assert!(lines.contains("System 2: disabled"));
}
