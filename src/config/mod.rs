//! Configuration: the `anchor.toml` model, filesystem layout, the interactive setup wizard and
//! Ollama provisioning.

mod file;
mod ollama;
pub mod provision;
mod validate;
pub mod wizard;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backlog::BacklogConfig;
use crate::base::{SourceDescriptor, SourceId, SourceKind, TrustLevel};
use crate::engine::embed::EmbeddingConfig;
use crate::engine::graph::GraphConfig;
use crate::engine::parser::ParserConfig;
use crate::engine::query::QueryConfig;
use crate::engine::router::ModelsConfig;
use crate::engine::watcher::WatcherConfig;
use crate::metrics::MetricsConfig;
use crate::sync::SyncConfig;
use crate::web::WebConfig;

pub use file::{
    EnvLookup, LoadedConfig, Secret, default_config_path_with, default_data_dir_with, process_env,
    resolve_secret,
};
pub use ollama::OllamaConfig;
pub use validate::{Issue, ValidationReport, is_valid_source_id};

/// How this installation operates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Runs locally: watches repositories and maintains a local graph. No server involved.
    #[default]
    Standalone,
    /// Like standalone, and additionally exchanges `.contextpack` deltas with a central server.
    Client,
    /// The 24/7 central sync server that merges deltas from clients.
    Server,
}

/// How much of a source is ingested: the zoom level of its "map tile".
///
/// Depth is ordered: `PhoneBook < Map < Full`. Queries that touch a source below `Full` are
/// answered from what is there and flagged as shallow.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    /// High-level entries only: the service, its owners and (for other kinds) nothing else.
    PhoneBook,
    /// Files, symbols with signatures and doc comments, document text; no call graph, no code.
    Map,
    /// Everything.
    #[default]
    Full,
}

impl Depth {
    /// Stable textual name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Depth::PhoneBook => "phone_book",
            Depth::Map => "map",
            Depth::Full => "full",
        }
    }

    /// Numeric code, increasing with depth.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::code`]; unknown codes are `Full`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Depth::PhoneBook,
            1 => Depth::Map,
            _ => Depth::Full,
        }
    }
}

impl std::fmt::Display for Depth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Depth {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "phone_book" | "phonebook" | "l0" => Ok(Depth::PhoneBook),
            "map" | "l1" => Ok(Depth::Map),
            "full" | "l2" => Ok(Depth::Full),
            other => Err(crate::Error::invalid(format!(
                "unknown depth `{other}` (expected phone_book, map or full)"
            ))),
        }
    }
}

/// One configured source (`[[sources]]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique identifier; also the sync namespace. Lowercase letters, digits, `-`, `_`, `.`.
    pub id: SourceId,
    /// What kind of system the source is.
    pub kind: SourceKind,
    /// Authority of the source: `source_of_truth` or `reference_only`.
    pub trust: TrustLevel,
    /// Directory, file or URL the source is read from.
    pub location: String,
    /// Human-readable name; defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How deep the source is ingested initially; it can be deepened later (`anchor deepen`).
    #[serde(default)]
    pub depth: Depth,
    /// Disabled sources are kept in the configuration but not ingested.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Glob patterns of paths to include (empty: everything).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Glob patterns of paths to exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Kind-specific options, interpreted by the ingester of that kind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl SourceConfig {
    /// A source with the default trust level for its kind and no extra options.
    pub fn new(id: impl Into<SourceId>, kind: SourceKind, location: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            trust: Self::default_trust(kind),
            location: location.into(),
            name: None,
            depth: Depth::Full,
            enabled: true,
            include: Vec::new(),
            exclude: Vec::new(),
            options: BTreeMap::new(),
        }
    }

    /// The trust level suggested for a kind of source: code, approved API definitions and the org
    /// model are authoritative; wikis, trackers and review discussions are contextual.
    pub fn default_trust(kind: SourceKind) -> TrustLevel {
        match kind {
            SourceKind::Repo | SourceKind::ApiPortal | SourceKind::OrgModel => {
                TrustLevel::SourceOfTruth
            }
            SourceKind::Wiki
            | SourceKind::Jira
            | SourceKind::CodeReview
            | SourceKind::Issues
            | SourceKind::Derived => TrustLevel::ReferenceOnly,
        }
    }

    /// Display name: the configured name or the id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(self.id.as_str())
    }

    /// The descriptor recorded in the graph store for this source.
    pub fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            id: self.id.clone(),
            kind: self.kind,
            name: self.display_name().to_owned(),
            trust: self.trust,
        }
    }
}

/// A GitHub repository whose issue board, discussion board and pull requests the daemon keeps
/// exported (`[[github]]`). Requires the GitHub CLI (`gh`) to be installed and logged in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubFetchConfig {
    /// `owner/name`.
    pub repository: String,
    /// Most recently updated issues to keep exported.
    #[serde(default = "default_issue_count")]
    pub issues: usize,
    /// Most recently updated discussions to keep exported.
    #[serde(default = "default_item_count")]
    pub discussions: usize,
    /// Most recently updated pull requests to keep exported.
    #[serde(default = "default_item_count")]
    pub pulls: usize,
    /// Minutes between refreshes while the daemon runs; `0` fetches only on `anchor fetch`.
    #[serde(default = "default_fetch_interval")]
    pub interval_minutes: u64,
    /// Export directory; defaults to `<data dir>/github/<owner>-<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<PathBuf>,
}

fn default_issue_count() -> usize {
    100
}
fn default_item_count() -> usize {
    50
}
fn default_fetch_interval() -> u64 {
    60
}

impl GithubFetchConfig {
    /// The export directory in effect.
    pub fn out_dir(&self, paths: &AnchorPaths) -> PathBuf {
        self.out.clone().unwrap_or_else(|| {
            paths
                .root()
                .join("github")
                .join(self.repository.replace('/', "-"))
        })
    }
}

/// Which flavour of Atlassian a [`AtlassianConfig`] talks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AtlassianDeployment {
    /// Atlassian Cloud (`*.atlassian.net`): basic auth with e-mail and API token; Confluence under
    /// `/wiki`, Jira REST v3.
    #[default]
    Cloud,
    /// Data Center / Server: personal access token as bearer token; Confluence and Jira REST
    /// without the `/wiki` prefix, Jira REST v2.
    #[value(name = "data_center")]
    DataCenter,
}

/// A Confluence and Jira site whose pages and issues the daemon keeps exported (`[[atlassian]]`).
///
/// Credentials are never stored here: `email_env` and `token_env` name environment variables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtlassianConfig {
    /// Site URL, e.g. `https://acme.atlassian.net` or `https://wiki.example.com` (include a
    /// context path such as `/confluence` for Data Center when there is one).
    pub base_url: String,
    /// `cloud` or `data_center`.
    #[serde(default)]
    pub deployment: AtlassianDeployment,
    /// Cloud only: name of the environment variable holding the account e-mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_env: Option<String>,
    /// Name of the environment variable holding the API token (Cloud) or personal access token
    /// (Data Center).
    pub token_env: String,
    /// Confluence space keys to export; `"*"` means every global space the token can see.
    #[serde(default)]
    pub confluence_spaces: Vec<String>,
    /// Jira project keys to export.
    #[serde(default)]
    pub jira_projects: Vec<String>,
    /// Extra JQL. Alone it selects the issues; together with `jira_projects` it narrows each
    /// project (`project = KEY AND (jql)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira_jql: Option<String>,
    /// Export directory; defaults to `<data dir>/atlassian/<host>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<PathBuf>,
    /// Minutes between refreshes while the daemon runs; `0` fetches only on demand.
    #[serde(default = "default_fetch_interval")]
    pub interval_minutes: u64,
    /// Export pages that have read restrictions (recorded as `restricted_to`). Off by default:
    /// restricted content would otherwise leak through shared packs and sync.
    #[serde(default)]
    pub include_restricted: bool,
    /// Most pages downloaded per run; `0` is unlimited. The next run continues where this stopped.
    #[serde(default)]
    pub max_pages: usize,
    /// Most issues downloaded per run; `0` is unlimited.
    #[serde(default)]
    pub max_issues: usize,
}

impl AtlassianConfig {
    /// The export directory in effect.
    pub fn out_dir(&self, paths: &AnchorPaths) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| paths.root().join("atlassian").join(self.host_slug()))
    }

    /// A file-name-safe rendering of the site (`acme.atlassian.net`, `wiki.example.com-8090`).
    pub fn host_slug(&self) -> String {
        let rest = self
            .base_url
            .split_once("://")
            .map_or(self.base_url.as_str(), |(_, rest)| rest);
        let rest = rest.rsplit('@').next().unwrap_or(rest);
        rest.trim_matches('/')
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

/// The complete configuration (`anchor.toml`). Every section is optional and defaults sensibly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnchorConfig {
    /// Operating mode.
    pub mode: Mode,
    /// Directory holding the database, keys, managed Ollama installation and exported packs.
    /// `None` uses [`default_data_dir`].
    pub data_dir: Option<PathBuf>,
    /// Model tiers and routing (`[models]`).
    pub models: ModelsConfig,
    /// Ollama backend (`[ollama]`).
    pub ollama: OllamaConfig,
    /// Embeddings (`[embeddings]`).
    pub embeddings: EmbeddingConfig,
    /// Code parser (`[parser]`).
    pub parser: ParserConfig,
    /// Git history ingestion (`[history]`).
    pub history: crate::engine::history::HistoryConfig,
    /// Graph traversal and trust weighting (`[graph]`).
    pub graph: GraphConfig,
    /// Query engine (`[query]`).
    pub query: QueryConfig,
    /// File watcher (`[watcher]`).
    pub watcher: WatcherConfig,
    /// Gap-analysis backlog (`[backlog]`).
    pub backlog: BacklogConfig,
    /// Token ledger (`[metrics]`).
    pub metrics: MetricsConfig,
    /// Client/server sync (`[sync]`).
    pub sync: SyncConfig,
    /// Dashboard (`[web]`).
    pub web: WebConfig,
    /// Configured sources (`[[sources]]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceConfig>,
    /// GitHub repositories kept exported by the daemon (`[[github]]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub github: Vec<GithubFetchConfig>,
    /// Confluence and Jira sites kept exported by the daemon (`[[atlassian]]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub atlassian: Vec<AtlassianConfig>,
}

impl AnchorConfig {
    /// The data directory in effect.
    pub fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(default_data_dir)
    }

    /// Filesystem layout under the data directory in effect.
    pub fn paths(&self) -> AnchorPaths {
        AnchorPaths::new(self.resolved_data_dir())
    }

    /// The configured source with the given id.
    pub fn source(&self, id: &SourceId) -> Option<&SourceConfig> {
        self.sources.iter().find(|source| &source.id == id)
    }
}

/// Environment variable overriding the data directory.
pub const ENV_HOME: &str = "ANCHOR_HOME";
/// Environment variable overriding the configuration file path.
pub const ENV_CONFIG: &str = "ANCHOR_CONFIG";

/// Default data directory in the real process environment; see [`default_data_dir_with`].
pub fn default_data_dir() -> PathBuf {
    default_data_dir_with(&process_env)
}

/// Default configuration file in the real process environment; see [`default_config_path_with`].
pub fn default_config_path() -> PathBuf {
    default_config_path_with(&process_env)
}

/// Filesystem layout of an Anchor data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorPaths {
    root: PathBuf,
}

impl AnchorPaths {
    /// Layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The SQLite database.
    pub fn db(&self) -> PathBuf {
        self.root.join("anchor.db")
    }

    /// The saved in-memory graph (a cache of the database; safe to delete).
    pub fn graph_snapshot(&self) -> PathBuf {
        self.root.join("graph.snapshot")
    }

    /// Device identity keys.
    pub fn keys_dir(&self) -> PathBuf {
        self.root.join("keys")
    }

    /// The Anchor-managed Ollama installation (binary, models, state, logs).
    pub fn ollama_dir(&self) -> PathBuf {
        self.root.join("ollama")
    }

    /// Downloaded embedding models.
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Exported and received `.contextpack` files.
    pub fn packs_dir(&self) -> PathBuf {
        self.root.join("packs")
    }

    /// Download cache.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Creates the data directory and its standard subdirectories.
    pub fn ensure(&self) -> crate::Result<()> {
        for dir in [
            self.root.clone(),
            self.keys_dir(),
            self.models_dir(),
            self.packs_dir(),
            self.cache_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_defaults() {
        let config: AnchorConfig = toml::from_str("").expect("empty config");
        assert_eq!(config, AnchorConfig::default());
        assert_eq!(config.mode, Mode::Standalone);
        assert_eq!(config.web.port, 7890);
        assert!(config.models.system1.is_enabled());
        assert!(!config.models.system2.is_enabled());
    }

    #[test]
    fn sources_parse_with_explicit_trust_levels() {
        let text = r#"
            mode = "client"

            [[sources]]
            id = "payments-api"
            kind = "repo"
            trust = "source_of_truth"
            location = "/src/payments-api"

            [[sources]]
            id = "eng-wiki"
            kind = "wiki"
            trust = "ReferenceOnly"
            location = "/exports/wiki"
            options = { scrape_comments = true }
        "#;
        let config: AnchorConfig = toml::from_str(text).expect("config");
        assert_eq!(config.mode, Mode::Client);
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].trust, TrustLevel::SourceOfTruth);
        assert_eq!(config.sources[1].trust, TrustLevel::ReferenceOnly);
        assert_eq!(
            config.sources[1].options.get("scrape_comments"),
            Some(&serde_json::Value::Bool(true))
        );
        let descriptor = config.sources[0].descriptor();
        assert_eq!(descriptor.name, "payments-api");
        assert!(config.source(&SourceId::new("eng-wiki")).is_some());
        assert!(config.source(&SourceId::new("missing")).is_none());
    }

    #[test]
    fn configuration_round_trips_through_toml() {
        let mut config = AnchorConfig::default();
        config.sources.push(SourceConfig::new(
            "portal",
            SourceKind::ApiPortal,
            "/exports/portal",
        ));
        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: AnchorConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(back, config);
    }

    #[test]
    fn default_trust_follows_source_kind() {
        for kind in [
            SourceKind::Repo,
            SourceKind::ApiPortal,
            SourceKind::OrgModel,
        ] {
            assert_eq!(SourceConfig::default_trust(kind), TrustLevel::SourceOfTruth);
        }
        for kind in [SourceKind::Wiki, SourceKind::Jira, SourceKind::CodeReview] {
            assert_eq!(SourceConfig::default_trust(kind), TrustLevel::ReferenceOnly);
        }
    }

    #[test]
    fn paths_are_rooted_and_creatable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AnchorPaths::new(dir.path().join("data"));
        paths.ensure().expect("ensure");
        assert!(paths.keys_dir().is_dir());
        assert!(paths.db().starts_with(paths.root()));
        assert!(paths.ollama_dir().ends_with("ollama"));
    }
}
