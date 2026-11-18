//! Semantic validation of an [`AnchorConfig`].

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use super::file::is_env_name;
use super::{AnchorConfig, AtlassianDeployment, Mode};
use crate::engine::embed::EmbeddingBackend;
use crate::engine::router::{ProviderKind, TierConfig};
use crate::sync::SyncTransport;

/// One finding about the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// Dotted path of the offending key, e.g. `sources[2].location` or `web.port`.
    pub path: String,
    /// What is wrong and, where possible, how to fix it.
    pub message: String,
}

impl Issue {
    /// Builds an issue.
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Outcome of [`AnchorConfig::validate`]. Errors block start-up; warnings do not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Problems that make the configuration unusable.
    pub errors: Vec<Issue>,
    /// Suspicious settings that Anchor can nevertheless run with.
    pub warnings: Vec<Issue>,
}

impl ValidationReport {
    /// `true` when there is no error (warnings are allowed).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// `Ok(())` when there is no error, otherwise an [`crate::Error::Config`] listing every error.
    pub fn into_result(self) -> crate::Result<()> {
        if self.is_ok() {
            return Ok(());
        }
        let lines: Vec<String> = self.errors.iter().map(Issue::to_string).collect();
        Err(crate::Error::config(format!(
            "invalid configuration:\n  {}",
            lines.join("\n  ")
        )))
    }

    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(Issue::new(path, message));
    }

    fn warning(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Issue::new(path, message));
    }
}

/// `true` for a well-formed source id: `[a-z0-9][a-z0-9._-]*`.
pub fn is_valid_source_id(id: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = id.chars();
    matches!(chars.next(), Some(first) if ok(first))
        && chars.all(|c| ok(c) || matches!(c, '.' | '_' | '-'))
}

/// Splits `scheme://rest`, lower-casing the scheme.
fn split_scheme(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let valid = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    (valid && !rest.is_empty()).then(|| (scheme.to_ascii_lowercase(), rest))
}

fn is_http_url(url: &str) -> bool {
    matches!(split_scheme(url), Some((scheme, _)) if scheme == "http" || scheme == "https")
}

/// `true` when the URL's host is a loopback name or address: such an endpoint is a local server,
/// which does not need an API key.
fn is_loopback_url(url: &str) -> bool {
    let Some((_, rest)) = split_scheme(url) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Port of a `host:port` listen address, when well formed.
fn listen_port(listen: &str) -> Option<u16> {
    if let Ok(address) = listen.parse::<SocketAddr>() {
        return Some(address.port());
    }
    let (host, port) = listen.rsplit_once(':')?;
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    host_ok.then(|| port.parse().ok()).flatten()
}

fn unit_range(report: &mut ValidationReport, path: &str, value: f32) {
    if !(0.0..=1.0).contains(&value) {
        report.error(path, format!("must be within 0..1, found {value}"));
    }
}

fn check_env_name(report: &mut ValidationReport, path: &str, name: &str) {
    if !is_env_name(name) {
        report.error(
            path,
            "must be the NAME of an environment variable holding the secret, not the secret itself",
        );
    }
}

fn validate_tier(report: &mut ValidationReport, path: &str, tier: &TierConfig) {
    // A disabled tier is never called: nothing about it is required.
    if tier.provider == ProviderKind::Disabled {
        return;
    }
    if tier.model.trim().is_empty() {
        report.error(
            format!("{path}.model"),
            "a model is required when the tier has a provider (or set provider = \"disabled\")",
        );
    }
    if let Some(url) = &tier.base_url
        && !is_http_url(url)
    {
        report.error(
            format!("{path}.base_url"),
            format!("`{url}` is not an http(s) URL"),
        );
    }
    match &tier.api_key_env {
        Some(name) => check_env_name(report, &format!("{path}.api_key_env"), name),
        None => {
            let local = tier.base_url.as_deref().is_some_and(is_loopback_url);
            let needs_key = match tier.provider {
                ProviderKind::Anthropic => true,
                ProviderKind::OpenAiCompatible => !local,
                ProviderKind::Ollama | ProviderKind::Disabled => false,
            };
            if needs_key {
                report.error(
                    format!("{path}.api_key_env"),
                    "a cloud provider needs api_key_env (the name of the environment variable holding the API key)",
                );
            }
        }
    }
    if tier.timeout_secs == 0 {
        report.error(format!("{path}.timeout_secs"), "must be greater than 0");
    }
    if tier.max_tokens == 0 {
        report.error(format!("{path}.max_tokens"), "must be greater than 0");
    }
    if !(0.0..=2.0).contains(&tier.temperature) {
        report.error(
            format!("{path}.temperature"),
            format!("must be within 0..2, found {}", tier.temperature),
        );
    }
}

impl AnchorConfig {
    /// Checks the configuration for inconsistencies. Pure apart from probing whether local source
    /// locations exist (reported as warnings: a repository may simply not be cloned yet).
    pub fn validate(&self) -> ValidationReport {
        let mut report = ValidationReport::default();
        self.validate_sources(&mut report);
        self.validate_models(&mut report);
        self.validate_ollama(&mut report);
        self.validate_network(&mut report);
        self.validate_tuning(&mut report);
        self.validate_atlassian(&mut report);
        if let Some(dir) = &self.data_dir
            && dir.as_os_str().is_empty()
        {
            report.error(
                "data_dir",
                "must not be empty; remove the key to use the default",
            );
        }
        report
    }

    fn validate_sources(&self, report: &mut ValidationReport) {
        let mut seen = BTreeSet::new();
        for (index, source) in self.sources.iter().enumerate() {
            let path = format!("sources[{index}]");
            let id = source.id.as_str();
            if !is_valid_source_id(id) {
                report.error(
                    format!("{path}.id"),
                    format!(
                        "`{id}` is not a valid source id: use lowercase letters, digits, `.`, `_`, `-`, starting with a letter or digit"
                    ),
                );
            }
            if !seen.insert(id) {
                report.error(format!("{path}.id"), format!("duplicate source id `{id}`"));
            }
            let location = source.location.trim();
            if location.is_empty() {
                report.error(format!("{path}.location"), "must not be empty");
            } else if split_scheme(location).is_none() && !Path::new(location).exists() {
                report.warning(
                    format!("{path}.location"),
                    format!("`{location}` does not exist; the source will be empty until it does"),
                );
            }
            for (field, patterns) in [("include", &source.include), ("exclude", &source.exclude)] {
                for (n, pattern) in patterns.iter().enumerate() {
                    if let Err(err) = globset::Glob::new(pattern) {
                        report.error(
                            format!("{path}.{field}[{n}]"),
                            format!("invalid glob: {err}"),
                        );
                    }
                }
            }
        }
    }

    fn validate_atlassian(&self, report: &mut ValidationReport) {
        let mut outs = BTreeSet::new();
        for (index, entry) in self.atlassian.iter().enumerate() {
            let path = format!("atlassian[{index}]");
            let url = entry.base_url.trim();
            if !is_http_url(url) {
                report.error(
                    format!("{path}.base_url"),
                    "must be an http(s) URL such as https://acme.atlassian.net",
                );
            } else if url.starts_with("http://") && !is_loopback_url(url) {
                report.error(
                    format!("{path}.base_url"),
                    "must use https: the credentials would travel unencrypted",
                );
            }
            check_env_name(report, &format!("{path}.token_env"), &entry.token_env);
            match (entry.deployment, entry.email_env.as_deref()) {
                (AtlassianDeployment::Cloud, None) => report.error(
                    format!("{path}.email_env"),
                    "Cloud uses e-mail + API token: name the variable holding the e-mail",
                ),
                (AtlassianDeployment::Cloud, Some(name)) => {
                    check_env_name(report, &format!("{path}.email_env"), name);
                }
                (AtlassianDeployment::DataCenter, Some(_)) => report.warning(
                    format!("{path}.email_env"),
                    "ignored: Data Center authenticates with the personal access token alone",
                ),
                (AtlassianDeployment::DataCenter, None) => {}
            }
            let jql = entry.jira_jql.as_deref().map(str::trim);
            if entry.confluence_spaces.is_empty()
                && entry.jira_projects.is_empty()
                && jql.is_none_or(str::is_empty)
            {
                report.error(
                    path.clone(),
                    "nothing to export: set confluence_spaces, jira_projects and/or jira_jql",
                );
            }
            if jql.is_some_and(str::is_empty) {
                report.error(format!("{path}.jira_jql"), "must not be empty when set");
            }
            for key in &entry.confluence_spaces {
                let valid = key == "*"
                    || (!key.is_empty()
                        && key.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '_' | '~' | '.' | '-')
                        }));
                if !valid {
                    report.error(
                        format!("{path}.confluence_spaces"),
                        format!("`{key}` is not a space key (letters, digits, `_ ~ . -`) or `*`"),
                    );
                }
            }
            for key in &entry.jira_projects {
                let mut chars = key.chars();
                let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    report.error(
                        format!("{path}.jira_projects"),
                        format!("`{key}` is not a Jira project key"),
                    );
                }
            }
            if !outs.insert(entry.out_dir(&self.paths())) {
                report.error(
                    format!("{path}.out"),
                    "two [[atlassian]] entries export into the same directory; set `out`",
                );
            }
        }
    }

    fn validate_models(&self, report: &mut ValidationReport) {
        validate_tier(report, "models.system1", &self.models.system1);
        validate_tier(report, "models.system2", &self.models.system2);
        let routing = &self.models.routing;
        unit_range(
            report,
            "models.routing.escalation_confidence",
            routing.escalation_confidence,
        );
        if let Some(budget) = routing.max_cloud_usd_per_day
            && !(budget.is_finite() && budget >= 0.0)
        {
            report.error(
                "models.routing.max_cloud_usd_per_day",
                "must be a non-negative amount",
            );
        }
        let system2 = &self.models.system2;
        if system2.is_enabled()
            && matches!(
                system2.provider,
                ProviderKind::Anthropic | ProviderKind::OpenAiCompatible
            )
            && !routing.allow_cloud
        {
            report.warning(
                "models.routing.allow_cloud",
                "system2 is a cloud provider but cloud use is disabled: queries will never escalate",
            );
        }
        for (index, price) in self.metrics.pricing.iter().enumerate() {
            for (field, value) in [
                ("input_usd_per_mtok", price.input_usd_per_mtok),
                ("output_usd_per_mtok", price.output_usd_per_mtok),
            ] {
                if !(value.is_finite() && value >= 0.0) {
                    report.error(
                        format!("metrics.pricing[{index}].{field}"),
                        "must be a non-negative price",
                    );
                }
            }
        }
    }

    fn validate_ollama(&self, report: &mut ValidationReport) {
        let ollama = &self.ollama;
        if let Err(err) = crate::ollama::normalize_base_url(&ollama.base_url) {
            report.error("ollama.base_url", err.to_string());
        }
        if ollama.startup_timeout_secs == 0 {
            report.error("ollama.startup_timeout_secs", "must be greater than 0");
        }
        if ollama.auto_provision && ollama.default_model.trim().is_empty() {
            report.error(
                "ollama.default_model",
                "a default model is required when auto_provision is enabled",
            );
        }
        if ollama.managed && !is_loopback_url(&format!("http://{}", strip_scheme(&ollama.base_url)))
        {
            report.warning(
                "ollama.base_url",
                "managed = true but the server is not on this machine; Anchor only manages a local installation",
            );
        }
    }

    fn validate_network(&self, report: &mut ValidationReport) {
        let web = &self.web;
        if web.enabled {
            if web.port == 0 {
                report.error("web.port", "must be within 1..65535");
            }
            let bind = web.bind.trim();
            if bind != "localhost" && bind.parse::<IpAddr>().is_err() {
                report.error(
                    "web.bind",
                    format!("`{bind}` is not an IP address or `localhost`"),
                );
            }
        }

        let sync = &self.sync;
        if let Some(url) = &sync.server_url {
            let expected: &[&str] = match sync.transport {
                SyncTransport::WebSocket => &["ws", "wss"],
                SyncTransport::Grpc => &["http", "https"],
            };
            match split_scheme(url) {
                Some((scheme, _)) if expected.contains(&scheme.as_str()) => {}
                Some((scheme, _)) => report.error(
                    "sync.server_url",
                    format!(
                        "scheme `{scheme}` does not match the {:?} transport: use {}",
                        sync.transport,
                        expected.join(":// or ") + "://"
                    ),
                ),
                None => report.error("sync.server_url", format!("`{url}` is not a URL")),
            }
        }
        match self.mode {
            Mode::Client => {
                if sync.server_url.is_none() {
                    report.error(
                        "sync.server_url",
                        "client mode requires the sync server URL",
                    );
                }
                if sync.auth_token_env.is_none() {
                    report.warning(
                        "sync.auth_token_env",
                        "no bearer token configured; the server will reject this client unless it allows anonymous access",
                    );
                }
            }
            Mode::Server => match listen_port(&sync.listen) {
                None | Some(0) => report.error(
                    "sync.listen",
                    format!("`{}` is not a host:port listen address", sync.listen),
                ),
                Some(port) if web.enabled && port == web.port => report.error(
                    "sync.listen",
                    format!("port {port} is also used by the dashboard (web.port)"),
                ),
                Some(_) => {}
            },
            Mode::Standalone => {}
        }
        if let Some(name) = &sync.auth_token_env {
            check_env_name(report, "sync.auth_token_env", name);
        }
        if self.mode != Mode::Standalone && sync.interval_secs == 0 {
            report.error("sync.interval_secs", "must be greater than 0");
        }
        for (field, ids) in [("publish", &sync.publish), ("subscribe", &sync.subscribe)] {
            for (index, id) in ids.iter().enumerate() {
                if self.source(id).is_none() {
                    report.error(
                        format!("sync.{field}[{index}]"),
                        format!("`{id}` is not a configured source id"),
                    );
                }
            }
        }
    }

    fn validate_tuning(&self, report: &mut ValidationReport) {
        let trust = &self.graph.trust;
        for (path, value) in [
            (
                "graph.trust.source_of_truth_multiplier",
                trust.source_of_truth_multiplier,
            ),
            (
                "graph.trust.reference_only_multiplier",
                trust.reference_only_multiplier,
            ),
            ("graph.max_cost", self.graph.max_cost),
        ] {
            if !(value.is_finite() && value > 0.0) {
                report.error(path, format!("must be a positive number, found {value}"));
            }
        }
        if trust.source_of_truth_multiplier > trust.reference_only_multiplier {
            report.warning(
                "graph.trust",
                "source_of_truth_multiplier exceeds reference_only_multiplier: authoritative data would cost more to traverse than reference material",
            );
        }
        if self.graph.max_hops == 0 {
            report.error("graph.max_hops", "must be at least 1");
        }
        if self.graph.max_visits == 0 {
            report.error("graph.max_visits", "must be at least 1");
        }
        if self.query.timeout_ms == 0 {
            report.warning(
                "query.timeout_ms",
                "0 disables the time budget: a pathological question can keep an agent waiting",
            );
        }
        unit_range(
            report,
            "query.confidence_threshold",
            self.query.confidence_threshold,
        );
        if self.query.top_k == 0 {
            report.error("query.top_k", "must be at least 1");
        }
        if self.query.max_context_tokens == 0 {
            report.error("query.max_context_tokens", "must be at least 1");
        }
        if self.embeddings.dimensions == 0 {
            report.error("embeddings.dimensions", "must be at least 1");
        }
        if self.embeddings.batch_size == 0 {
            report.error("embeddings.batch_size", "must be at least 1");
        }
        if self.embeddings.backend != EmbeddingBackend::Hash
            && self.embeddings.model.trim().is_empty()
        {
            report.error(
                "embeddings.model",
                "a model is required for the candle and ollama backends",
            );
        }
        if self.backlog.enabled && self.backlog.worker_interval_secs == 0 {
            report.error("backlog.worker_interval_secs", "must be greater than 0");
        }
    }
}

fn strip_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{SourceId, SourceKind};
    use crate::config::SourceConfig;

    fn has(issues: &[Issue], path: &str) -> bool {
        issues.iter().any(|issue| issue.path == path)
    }

    #[test]
    fn default_configuration_is_valid() {
        let report = AnchorConfig::default().validate();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.into_result().is_ok());
    }

    #[test]
    fn atlassian_rules() {
        use crate::config::{AtlassianConfig, AtlassianDeployment};
        let good = AtlassianConfig {
            base_url: "https://acme.atlassian.net".into(),
            deployment: AtlassianDeployment::Cloud,
            email_env: Some("ATL_EMAIL".into()),
            token_env: "ATL_TOKEN".into(),
            confluence_spaces: vec!["ENG".into(), "*".into()],
            jira_projects: vec!["PAY".into()],
            jira_jql: None,
            out: None,
            interval_minutes: 60,
            include_restricted: false,
            max_pages: 0,
            max_issues: 0,
        };
        let check = |entry: AtlassianConfig| {
            AnchorConfig {
                atlassian: vec![entry],
                ..AnchorConfig::default()
            }
            .validate()
        };
        assert!(check(good.clone()).is_ok());

        let report = check(AtlassianConfig {
            base_url: "http://acme.example".into(),
            email_env: None,
            token_env: "not an env name=abc123".into(),
            confluence_spaces: vec!["bad key!".into()],
            jira_projects: vec!["1x".into()],
            ..good.clone()
        });
        for path in [
            "atlassian[0].base_url",
            "atlassian[0].email_env",
            "atlassian[0].token_env",
            "atlassian[0].confluence_spaces",
            "atlassian[0].jira_projects",
        ] {
            assert!(has(&report.errors, path), "{path}: {:?}", report.errors);
        }
        let report = check(AtlassianConfig {
            confluence_spaces: vec![],
            jira_projects: vec![],
            ..good.clone()
        });
        assert!(has(&report.errors, "atlassian[0]"), "nothing to export");
        let data_center = check(AtlassianConfig {
            deployment: AtlassianDeployment::DataCenter,
            base_url: "http://localhost:8090".into(),
            ..good
        });
        assert!(data_center.is_ok(), "{:?}", data_center.errors);
        assert!(has(&data_center.warnings, "atlassian[0].email_env"));
    }

    #[test]
    fn source_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let existing = dir.path().to_string_lossy().into_owned();
        let mut config = AnchorConfig {
            sources: vec![
                SourceConfig::new("ok.repo_1", SourceKind::Repo, existing.clone()),
                SourceConfig::new("Bad Id", SourceKind::Repo, existing.clone()),
                SourceConfig::new(
                    "ok.repo_1",
                    SourceKind::Wiki,
                    dir.path().join("missing").to_string_lossy(),
                ),
                SourceConfig::new("remote", SourceKind::Jira, "https://jira.example.test"),
                SourceConfig::new("-lead", SourceKind::Jira, " "),
            ],
            ..AnchorConfig::default()
        };
        config.sources[0].include = vec!["src/{a".to_owned()];
        let report = config.validate();
        assert!(has(&report.errors, "sources[1].id"));
        assert!(has(&report.errors, "sources[2].id"), "duplicate");
        assert!(has(&report.warnings, "sources[2].location"));
        assert!(has(&report.errors, "sources[4].id"));
        assert!(has(&report.errors, "sources[4].location"));
        assert!(has(&report.errors, "sources[0].include[0]"));
        assert!(!has(&report.warnings, "sources[0].location"));
        assert!(
            !has(&report.warnings, "sources[3].location"),
            "URLs are not probed"
        );
        assert_eq!(report.errors.len(), 5, "{:?}", report.errors);
        let err = report.into_result().expect_err("errors block");
        assert!(err.to_string().contains("sources[1].id"));
    }

    #[test]
    fn tier_rules() {
        let mut config = AnchorConfig::default();
        // A disabled tier has no requirements at all.
        config.models.system2.provider = ProviderKind::Disabled;
        config.models.system2.model = String::new();
        config.models.system2.timeout_secs = 0;
        assert!(config.validate().is_ok());

        config.models.system2.provider = ProviderKind::Anthropic;
        config.models.routing.escalation_confidence = 1.5;
        let report = config.validate();
        for path in [
            "models.system2.model",
            "models.system2.api_key_env",
            "models.system2.timeout_secs",
            "models.routing.escalation_confidence",
        ] {
            assert!(has(&report.errors, path), "{path}: {:?}", report.errors);
        }

        let mut config = AnchorConfig::default();
        config.models.system2.provider = ProviderKind::OpenAiCompatible;
        config.models.system2.model = "m".to_owned();
        config.models.system2.base_url = Some("http://localhost:1234/v1".to_owned());
        config.models.routing.allow_cloud = true;
        assert!(config.validate().is_ok(), "a local endpoint needs no key");
        config.models.system2.base_url = Some("https://api.example.test/v1".to_owned());
        assert!(has(&config.validate().errors, "models.system2.api_key_env"));
        config.models.system2.api_key_env = Some("sk-not-a-name!".to_owned());
        assert!(has(&config.validate().errors, "models.system2.api_key_env"));
        config.models.system2.api_key_env = Some("LLM_KEY".to_owned());
        config.models.routing.allow_cloud = false;
        let report = config.validate();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(has(&report.warnings, "models.routing.allow_cloud"));
    }

    #[test]
    fn sync_and_port_rules() {
        let mut config = AnchorConfig {
            mode: Mode::Client,
            ..AnchorConfig::default()
        };
        let report = config.validate();
        assert!(has(&report.errors, "sync.server_url"));

        config.sync.server_url = Some("https://sync.example.test".to_owned());
        config.sync.transport = SyncTransport::WebSocket;
        assert!(
            has(&config.validate().errors, "sync.server_url"),
            "scheme mismatch"
        );
        config.sync.transport = SyncTransport::Grpc;
        config.sync.publish = vec![SourceId::new("ghost")];
        let report = config.validate();
        assert!(!has(&report.errors, "sync.server_url"));
        assert!(has(&report.warnings, "sync.auth_token_env"));
        assert!(has(&report.errors, "sync.publish[0]"));

        let mut server = AnchorConfig {
            mode: Mode::Server,
            ..AnchorConfig::default()
        };
        server.sync.listen = format!("0.0.0.0:{}", server.web.port);
        assert!(has(&server.validate().errors, "sync.listen"), "port clash");
        server.sync.listen = "nonsense".to_owned();
        assert!(has(&server.validate().errors, "sync.listen"));
        server.web.port = 0;
        server.web.bind = "not an ip".to_owned();
        let report = server.validate();
        assert!(has(&report.errors, "web.port"));
        assert!(has(&report.errors, "web.bind"));
    }

    #[test]
    fn numeric_ranges() {
        let mut config = AnchorConfig::default();
        config.graph.trust.source_of_truth_multiplier = 0.0;
        config.graph.trust.reference_only_multiplier = -1.0;
        config.graph.max_hops = 0;
        config.graph.max_visits = 0;
        config.query.timeout_ms = 0;
        config.query.confidence_threshold = -0.1;
        config.query.top_k = 0;
        config.embeddings.dimensions = 0;
        config.ollama.startup_timeout_secs = 0;
        config.ollama.base_url = "http://".to_owned();
        let report = config.validate();
        for path in [
            "graph.trust.source_of_truth_multiplier",
            "graph.trust.reference_only_multiplier",
            "graph.max_hops",
            "graph.max_visits",
            "query.confidence_threshold",
            "query.top_k",
            "embeddings.dimensions",
            "ollama.startup_timeout_secs",
            "ollama.base_url",
        ] {
            assert!(has(&report.errors, path), "{path}: {:?}", report.errors);
        }
        assert!(has(&report.warnings, "query.timeout_ms"));
        assert!(!has(
            &AnchorConfig::default().validate().warnings,
            "query.timeout_ms"
        ));
    }
}
