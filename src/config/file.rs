//! Reading and writing `anchor.toml`, unknown-key detection, environment-driven defaults and
//! secret resolution.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::validate::{Issue, ValidationReport};
use super::{AnchorConfig, ENV_CONFIG, ENV_HOME, SourceConfig};
use crate::base::SourceKind;
use crate::metrics::ModelPrice;
use crate::{Error, Result};

/// An environment lookup: variable name to value. Injected everywhere the environment matters so
/// that tests never mutate the process environment.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The real process environment as an [`EnvLookup`].
pub fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// Header written at the top of every saved configuration file.
const HEADER: &str = "# Anchor configuration, written by `anchor init`, `anchor setup` and `anchor source add`; safe to\n# edit by hand. Every setting is optional: what is left out takes its default.\n\
# Secrets never live here: API keys and tokens are read from the environment variables named below.\n\n";

/// Appended while no source is configured, so the first one can be added by uncommenting.
const SOURCES_EXAMPLE: &str = "
# Sources: what Anchor indexes. `anchor source add <path>` adds one for you (it infers the kind
# and trust level); or uncomment and edit:
#
# [[sources]]
# id = \"payments-api\"
# kind = \"repo\"                  # repo | wiki | api_portal | jira | org_model | code_review | issues
# trust = \"source_of_truth\"      # or reference_only (wikis, tickets, discussions)
# location = \"/path/to/payments-api\"
";

/// A configuration file read from disk together with what was noticed while reading it.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedConfig {
    /// The parsed configuration.
    pub config: AnchorConfig,
    /// One warning per key present in the file that Anchor does not know.
    pub unknown_keys: Vec<Issue>,
}

impl LoadedConfig {
    /// Validation of the configuration, with the unknown keys prepended to the warnings.
    pub fn report(&self) -> ValidationReport {
        let mut report = self.config.validate();
        let mut warnings = self.unknown_keys.clone();
        warnings.append(&mut report.warnings);
        report.warnings = warnings;
        report
    }
}

impl AnchorConfig {
    /// Parses configuration text. Unknown keys are tolerated (see [`AnchorConfig::unknown_keys`]).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Reads the configuration at `path`. A missing or malformed file is an error naming the path.
    /// Unknown keys are logged as warnings; use [`AnchorConfig::load_with_report`] to get them.
    pub fn load(path: &Path) -> Result<Self> {
        let loaded = Self::load_with_report(path)?;
        for issue in &loaded.unknown_keys {
            tracing::warn!(file = %path.display(), key = %issue.path, "{}", issue.message);
        }
        Ok(loaded.config)
    }

    /// Reads the configuration at `path` and reports the keys of the file Anchor does not know.
    pub fn load_with_report(path: &Path) -> Result<LoadedConfig> {
        let text = std::fs::read_to_string(path).map_err(|err| {
            Error::config(format!(
                "cannot read configuration {}: {err}",
                path.display()
            ))
        })?;
        let config = Self::from_toml_str(&text)
            .map_err(|err| Error::config(format!("{}: {}", path.display(), err.detail())))?;
        let unknown_keys = Self::unknown_keys(&text);
        Ok(LoadedConfig {
            config,
            unknown_keys,
        })
    }

    /// Like [`AnchorConfig::load`], but a file that does not exist yields the defaults.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// The configuration as pretty TOML, without the file header.
    pub fn to_toml_string(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Writes the configuration to `path` atomically: the text goes to a temporary file in the same
    /// directory, which is then renamed over the destination, so a crash never leaves a truncated
    /// configuration. Parent directories are created.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = format!("{HEADER}{}", self.to_toml_string()?);
        if self.sources.is_empty() {
            text.push_str(SOURCES_EXAMPLE);
        }
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&parent)?;
        let mut file = tempfile::NamedTempFile::new_in(&parent)?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|err| Error::Io(err.error))?;
        Ok(())
    }

    /// Keys present in `text` that are not part of the configuration schema, as warnings with their
    /// dotted path (`web.prot`, `sources[1].colour`). Text that is not valid TOML yields nothing:
    /// parsing reports that error.
    ///
    /// The schema is derived from the serialised form of a fully populated configuration, so it
    /// follows the configuration structs without a hand-maintained key list.
    pub fn unknown_keys(text: &str) -> Vec<Issue> {
        let Ok(actual) = text.parse::<toml::Table>() else {
            return Vec::new();
        };
        let Ok(mut schema) = toml::Table::try_from(schema_exemplar()) else {
            return Vec::new();
        };
        // Lists left empty in the exemplar are not serialised, but their keys are valid.
        for key in ["sources", "github", "atlassian"] {
            schema
                .entry(key)
                .or_insert_with(|| toml::Value::Array(Vec::new()));
        }
        let mut issues = Vec::new();
        diff_tables(&actual, &schema, "", &mut issues);
        issues
    }
}

/// A configuration in which every optional key is present, so that its serialised form lists the
/// whole schema (TOML has no null: `None` fields are simply absent from the default configuration).
fn schema_exemplar() -> AnchorConfig {
    let mut config = AnchorConfig {
        data_dir: Some(PathBuf::from("/")),
        ..AnchorConfig::default()
    };
    for tier in [&mut config.models.system1, &mut config.models.system2] {
        tier.base_url = Some(String::new());
        tier.api_key_env = Some(String::new());
    }
    config.models.routing.max_cloud_usd_per_day = Some(0.0);
    config.sync.server_url = Some(String::new());
    config.sync.auth_token_env = Some(String::new());
    config.sync.publish = vec!["x".into()];
    config.sync.subscribe = vec!["x".into()];
    config.metrics.pricing = vec![ModelPrice {
        provider: String::new(),
        model: String::new(),
        input_usd_per_mtok: 0.0,
        output_usd_per_mtok: 0.0,
        long_prompt_threshold_tokens: None,
        long_input_usd_per_mtok: None,
        long_output_usd_per_mtok: None,
    }];
    let mut source = SourceConfig::new("x", SourceKind::Repo, "/");
    source.name = Some(String::new());
    source.include = vec![String::new()];
    source.exclude = vec![String::new()];
    config.sources = vec![source];
    config
}

/// Free-form tables whose keys are not part of the schema.
fn is_free_form(path: &str) -> bool {
    path.starts_with("sources[") && path.ends_with("].options")
}

fn diff_tables(actual: &toml::Table, schema: &toml::Table, prefix: &str, out: &mut Vec<Issue>) {
    for (key, value) in actual {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if is_free_form(&path) {
            continue;
        }
        let Some(expected) = schema.get(key) else {
            out.push(Issue::new(
                path,
                "unknown configuration key (ignored); check the spelling",
            ));
            continue;
        };
        match (value, expected) {
            (toml::Value::Table(actual), toml::Value::Table(expected)) => {
                diff_tables(actual, expected, &path, out);
            }
            (toml::Value::Array(items), toml::Value::Array(expected)) => {
                if let Some(toml::Value::Table(expected)) = expected.first() {
                    for (index, item) in items.iter().enumerate() {
                        if let toml::Value::Table(item) = item {
                            diff_tables(item, expected, &format!("{path}[{index}]"), out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "anchor", "anchor")
}

fn non_empty(lookup: EnvLookup<'_>, name: &str) -> Option<OsString> {
    lookup(name).filter(|value| !value.is_empty())
}

/// Default data directory given an environment: `$ANCHOR_HOME`, else the platform's local data
/// directory (`~/.local/share/anchor` on Linux), else `./.anchor`.
pub fn default_data_dir_with(lookup: EnvLookup<'_>) -> PathBuf {
    if let Some(home) = non_empty(lookup, ENV_HOME) {
        return PathBuf::from(home);
    }
    project_dirs()
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".anchor"))
}

/// Default configuration file given an environment: `$ANCHOR_CONFIG`, else
/// `$ANCHOR_HOME/anchor.toml` when `ANCHOR_HOME` is set, else the platform's configuration
/// directory (`~/.config/anchor/anchor.toml` on Linux), else `./anchor.toml`.
pub fn default_config_path_with(lookup: EnvLookup<'_>) -> PathBuf {
    if let Some(path) = non_empty(lookup, ENV_CONFIG) {
        return PathBuf::from(path);
    }
    if let Some(home) = non_empty(lookup, ENV_HOME) {
        return PathBuf::from(home).join("anchor.toml");
    }
    project_dirs()
        .map(|dirs| dirs.config_dir().join("anchor.toml"))
        .unwrap_or_else(|| PathBuf::from("anchor.toml"))
}

/// A secret value (API key, token). Its `Debug` output is redacted and it is never serialised, so it
/// cannot leak into logs or configuration files by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The secret itself. Call this only at the point where the value is sent to its service.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Resolves the secret stored in the environment variable `env_name`.
///
/// Errors name the variable, never a value: the variable name is invalid, the variable is unset or
/// blank, or its value is not valid UTF-8. Surrounding whitespace (a trailing newline from
/// `export KEY=$(cat file)`) is trimmed.
pub fn resolve_secret(env_name: &str, lookup: EnvLookup<'_>) -> Result<Secret> {
    if !is_env_name(env_name) {
        return Err(Error::config(format!(
            "`{env_name}` is not a valid environment variable name"
        )));
    }
    let value = lookup(env_name).ok_or_else(|| {
        Error::config(format!(
            "environment variable {env_name} is not set; export it before starting Anchor"
        ))
    })?;
    let value = value.into_string().map_err(|_| {
        Error::config(format!(
            "environment variable {env_name} is not valid UTF-8"
        ))
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::config(format!(
            "environment variable {env_name} is empty"
        )));
    }
    Ok(Secret(value.to_owned()))
}

/// `true` for a portable environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub(crate) fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn save_is_atomic_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/deeper/anchor.toml");
        let mut config = AnchorConfig {
            mode: Mode::Client,
            data_dir: Some(dir.path().join("data")),
            ..AnchorConfig::default()
        };
        config.sync.server_url = Some("wss://sync.example.test/ws".to_owned());
        config
            .sources
            .push(SourceConfig::new("wiki", SourceKind::Wiki, "/exports/wiki"));
        config.save(&path).expect("save");
        // Overwriting an existing file works and leaves no temporary file behind.
        config.ollama.managed = true;
        config.save(&path).expect("save again");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# Anchor configuration"));
        let loaded = AnchorConfig::load_with_report(&path).expect("load");
        assert_eq!(loaded.config, config);
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        let entries: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .collect();
        assert_eq!(entries.len(), 1, "only anchor.toml remains");
    }

    #[test]
    fn missing_and_malformed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("anchor.toml");
        assert!(AnchorConfig::load(&missing).is_err());
        assert_eq!(
            AnchorConfig::load_or_default(&missing).expect("default"),
            AnchorConfig::default()
        );
        std::fs::write(&missing, "mode = [").expect("write");
        let err = AnchorConfig::load_or_default(&missing).expect_err("malformed");
        assert!(err.to_string().contains("anchor.toml"), "{err}");
    }

    #[test]
    fn list_sections_are_known_keys_even_when_empty_in_the_defaults() {
        let text = "[[github]]\nrepository = \"o/r\"\n\n[[atlassian]]\nbase_url = \"https://x.atlassian.net\"\n";
        let issues = AnchorConfig::unknown_keys(text);
        assert!(
            !issues
                .iter()
                .any(|issue| issue.path == "github" || issue.path == "atlassian"),
            "{issues:?}"
        );
        let saved = AnchorConfig::default().to_toml_string().unwrap();
        assert!(!saved.contains("sources = []"), "{saved}");
    }

    #[test]
    fn unknown_keys_are_warnings_not_errors() {
        let text = r#"
            mode = "standalone"
            colour = "blue"
            data_dir = "/tmp/x"

            [web]
            prot = 1

            [models.system2]
            api_key_env = "KEY"
            flavour = 1

            [sync]
            server_url = "ws://h/ws"

            [[sources]]
            id = "a"
            kind = "repo"
            trust = "source_of_truth"
            location = "/a"
            name = "A"
            options = { anything = { goes = true } }

            [[sources]]
            id = "b"
            kind = "wiki"
            trust = "reference_only"
            location = "/b"
            colour = "red"
        "#;
        let config = AnchorConfig::from_toml_str(text).expect("unknown keys are not fatal");
        assert_eq!(config.sources.len(), 2);
        let paths: Vec<String> = AnchorConfig::unknown_keys(text)
            .into_iter()
            .map(|issue| issue.path)
            .collect();
        assert_eq!(
            paths,
            [
                "colour",
                "models.system2.flavour",
                "sources[1].colour",
                "web.prot"
            ]
        );
        // The default configuration and the exemplar themselves are fully known.
        let default_text = AnchorConfig::default().to_toml_string().expect("toml");
        assert!(AnchorConfig::unknown_keys(&default_text).is_empty());
    }

    #[test]
    fn defaults_follow_the_injected_environment() {
        let both = env(&[
            ("ANCHOR_HOME", "/srv/anchor"),
            ("ANCHOR_CONFIG", "/etc/a.toml"),
        ]);
        assert_eq!(default_data_dir_with(&both), PathBuf::from("/srv/anchor"));
        assert_eq!(
            default_config_path_with(&both),
            PathBuf::from("/etc/a.toml")
        );
        let home = env(&[("ANCHOR_HOME", "/srv/anchor"), ("ANCHOR_CONFIG", "")]);
        assert_eq!(
            default_config_path_with(&home),
            PathBuf::from("/srv/anchor/anchor.toml")
        );
        let none = env(&[]);
        assert!(default_config_path_with(&none).ends_with("anchor.toml"));
        assert!(!default_data_dir_with(&none).as_os_str().is_empty());
    }

    #[test]
    fn secrets_resolve_from_the_lookup_and_stay_redacted() {
        let lookup = env(&[("API_KEY", "  s3cr3t\n"), ("BLANK", "  ")]);
        let secret = resolve_secret("API_KEY", &lookup).expect("secret");
        assert_eq!(secret.expose(), "s3cr3t");
        assert!(!format!("{secret:?}").contains("s3cr3t"));
        for name in ["MISSING", "BLANK", "not a name", ""] {
            let err = resolve_secret(name, &lookup).expect_err(name);
            assert!(!err.to_string().contains("s3cr3t"));
        }
    }
}
