//! Configuration: the `anchor.toml` model, filesystem layout, the interactive setup wizard and
//! Ollama provisioning.

mod ollama;

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

pub use ollama::OllamaConfig;

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
            SourceKind::Wiki | SourceKind::Jira | SourceKind::CodeReview => {
                TrustLevel::ReferenceOnly
            }
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
    pub sources: Vec<SourceConfig>,
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

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "anchor", "anchor")
}

/// Default data directory: `$ANCHOR_HOME`, else the platform's local data directory
/// (`~/.local/share/anchor` on Linux), else `./.anchor`.
pub fn default_data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os(ENV_HOME).filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    project_dirs()
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".anchor"))
}

/// Default configuration file: `$ANCHOR_CONFIG`, else `$ANCHOR_HOME/anchor.toml` when
/// `ANCHOR_HOME` is set, else the platform's configuration directory
/// (`~/.config/anchor/anchor.toml` on Linux), else `./anchor.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_CONFIG).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os(ENV_HOME).filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join("anchor.toml");
    }
    project_dirs()
        .map(|dirs| dirs.config_dir().join("anchor.toml"))
        .unwrap_or_else(|| PathBuf::from("anchor.toml"))
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
