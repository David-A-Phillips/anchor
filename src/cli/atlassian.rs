//! `anchor fetch-atlassian`: exports live Confluence pages and Jira issues into the on-disk layouts
//! the `wiki` and `jira` ingesters read.
//!
//! * Confluence (REST v1, `/wiki/rest/api` on Cloud, `/rest/api` on Data Center): every page of
//!   the configured spaces becomes `<out>/confluence/<SPACE>/<page id>.html` (metadata in
//!   `<meta>` tags) plus `<page id>.comments.json` with footer and inline comment threads.
//! * Jira (`/rest/api/3/search/jql` on Cloud, `/rest/api/2/search` on Data Center): every issue
//!   becomes `<out>/jira/<PROJECT>/<KEY>.json` in the REST issue shape.
//! * `<out>/.atlassian-state.json` records, per space and project, what is exported and the
//!   `updated` high-water mark. A new run only asks for items modified since (minus a one day
//!   safety margin, see [`SAFETY_MARGIN_MS`]) and downloads only those whose `updated` differs from
//!   the recorded one. The mark advances after every item is written, so an interrupted run
//!   resumes where it stopped.
//!
//! Pages with read restrictions are skipped unless [`AtlassianOptions::include_restricted`] is set;
//! see `docs/atlassian.md`.

mod command;
mod confluence;
mod http;
mod jira;
pub mod storage;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::base::net::http_client;
use crate::base::time::parse_timestamp_ms;
use crate::base::{Error, Result};
use crate::config::{AnchorPaths, AtlassianConfig, AtlassianDeployment};

pub use command::{FetchAtlassianArgs, run_command, spawn_refreshers};
pub use http::{Credentials, RetryPolicy, Sleeper, TokioSleeper};

/// Incremental queries start this long before the recorded high-water mark. CQL and JQL compare
/// minute-precision local times in the *user's* time zone, so a one day margin keeps every zone
/// safe; items inside the margin are skipped again by comparing their `updated` value.
pub const SAFETY_MARGIN_MS: i64 = 24 * 60 * 60 * 1000;

/// File name of the incremental state inside the export directory (hidden: never ingested).
pub const STATE_FILE: &str = ".atlassian-state.json";

/// What to export.
#[derive(Clone, Debug)]
pub struct AtlassianOptions {
    /// Site URL without trailing slash.
    pub base_url: String,
    /// Cloud or Data Center.
    pub deployment: AtlassianDeployment,
    /// Credentials (never printed).
    pub credentials: Credentials,
    /// Names of the environment variables the credentials came from, for error messages.
    pub auth_hint: String,
    /// Confluence space keys, or `*`.
    pub spaces: Vec<String>,
    /// Jira project keys.
    pub projects: Vec<String>,
    /// Extra JQL.
    pub jql: Option<String>,
    /// Export directory.
    pub out: PathBuf,
    /// Export restricted pages too.
    pub include_restricted: bool,
    /// Most pages downloaded per run (`0` = unlimited).
    pub max_pages: usize,
    /// Most issues downloaded per run (`0` = unlimited).
    pub max_issues: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Retry behaviour.
    pub retry: RetryPolicy,
}

impl AtlassianOptions {
    /// Options for a configured site, reading the credentials from the environment through `env`.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a required one is unset or empty.
    pub fn from_config(
        config: &AtlassianConfig,
        paths: &AnchorPaths,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let read = |field: &str, name: &str| -> Result<String> {
            env(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::config(format!(
                        "environment variable `{name}` ({field} of [[atlassian]] {}) is not set",
                        config.base_url
                    ))
                })
        };
        let token = read("token_env", &config.token_env)?;
        let (credentials, auth_hint) = match config.deployment {
            AtlassianDeployment::Cloud => {
                let email_env = config.email_env.as_deref().ok_or_else(|| {
                    Error::config(format!(
                        "[[atlassian]] {} is a Cloud site: set email_env",
                        config.base_url
                    ))
                })?;
                let email = read("email_env", email_env)?;
                (
                    Credentials::Basic { email, token },
                    format!(
                        "the {email_env} and {} environment variables",
                        config.token_env
                    ),
                )
            }
            AtlassianDeployment::DataCenter => (
                Credentials::Bearer { token },
                format!("the {} environment variable", config.token_env),
            ),
        };
        Ok(Self {
            base_url: config.base_url.trim().trim_end_matches('/').to_owned(),
            deployment: config.deployment,
            credentials,
            auth_hint,
            spaces: config.confluence_spaces.clone(),
            projects: config.jira_projects.clone(),
            jql: config.jira_jql.clone().filter(|jql| !jql.trim().is_empty()),
            out: config.out_dir(paths),
            include_restricted: config.include_restricted,
            max_pages: config.max_pages,
            max_issues: config.max_issues,
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
        })
    }

    /// Directory the `wiki` source should point at.
    pub fn wiki_dir(&self) -> PathBuf {
        self.out.join("confluence")
    }

    /// Directory the `jira` source should point at.
    pub fn jira_dir(&self) -> PathBuf {
        self.out.join("jira")
    }

    pub(crate) fn confluence_prefix(&self) -> &'static str {
        match self.deployment {
            AtlassianDeployment::Cloud => "/wiki/rest/api",
            AtlassianDeployment::DataCenter => "/rest/api",
        }
    }

    pub(crate) fn jira_prefix(&self) -> &'static str {
        match self.deployment {
            AtlassianDeployment::Cloud => "/rest/api/3",
            AtlassianDeployment::DataCenter => "/rest/api/2",
        }
    }
}

/// What a run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlassianReport {
    /// Confluence pages written (new or changed).
    pub pages: usize,
    /// Comments written across those pages.
    pub comments: usize,
    /// Jira issues written (new or changed).
    pub issues: usize,
    /// Pages and issues listed again but unchanged.
    pub unchanged: usize,
    /// Distinct pages skipped because of read restrictions (directly or inherited).
    pub restricted_skipped: usize,
    /// Pages removed from the export because they were deleted upstream (or became restricted).
    pub removed: usize,
    /// A `max_pages` / `max_issues` cap stopped the run early; run again to continue.
    pub truncated: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct State {
    #[serde(default)]
    pub(crate) confluence: BTreeMap<String, SpaceState>,
    #[serde(default)]
    pub(crate) jira: BTreeMap<String, ProjectState>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SpaceState {
    pub(crate) high_water: Option<String>,
    /// Whether the last run exported restricted pages (changing it restarts the listing).
    #[serde(default)]
    pub(crate) included_restricted: bool,
    pub(crate) pages: BTreeMap<String, PageEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PageEntry {
    pub(crate) version: u64,
    pub(crate) updated: String,
    /// Ancestor page ids, root first: a restriction on any of them restricts this page.
    pub(crate) ancestors: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct ProjectState {
    pub(crate) high_water: Option<String>,
    /// Issue key to its exported `updated` value.
    pub(crate) issues: BTreeMap<String, String>,
}

/// Shared state of one run.
pub(crate) struct Run<'a> {
    pub(crate) api: http::Api<'a>,
    pub(crate) options: &'a AtlassianOptions,
    pub(crate) state: State,
    pub(crate) report: AtlassianReport,
}

impl Run<'_> {
    /// Persists the state (atomically); called after every exported item.
    pub(crate) fn checkpoint(&self) -> Result<()> {
        write_atomic(
            &self.options.out.join(STATE_FILE),
            &serde_json::to_vec_pretty(&self.state)?,
        )
    }
}

/// Exports the configured spaces and projects. Safe to interrupt and repeat.
///
/// # Errors
/// Authentication, transport (after retries), parse and I/O failures. Items written before the
/// failure stay, and the next run continues after them.
pub async fn fetch_atlassian(
    options: &AtlassianOptions,
    sleeper: &dyn Sleeper,
) -> Result<AtlassianReport> {
    std::fs::create_dir_all(&options.out)?;
    let state = match std::fs::read(options.out.join(STATE_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!("ignoring unreadable {STATE_FILE}: {err}; exporting from scratch");
            State::default()
        }),
        Err(_) => State::default(),
    };
    let mut run = Run {
        api: http::Api {
            http: http_client(Some(options.timeout))?,
            base: options.base_url.clone(),
            credentials: &options.credentials,
            auth_hint: options.auth_hint.clone(),
            sleeper,
            retry: options.retry,
        },
        options,
        state,
        report: AtlassianReport::default(),
    };
    if !options.spaces.is_empty() {
        confluence::export(&mut run).await?;
    }
    if !options.projects.is_empty() || options.jql.is_some() {
        jira::export(&mut run).await?;
    }
    run.checkpoint()?;
    Ok(run.report)
}

/// Writes `bytes` to `path` through a `.part` file, creating parent directories.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, bytes)?;
    std::fs::rename(partial, path)?;
    Ok(())
}

/// A name usable as a single path component.
pub(crate) fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "_".to_owned()
    } else {
        cleaned
    }
}

/// `yyyy-MM-dd HH:mm` (UTC) of `high_water` minus the safety margin, as CQL and JQL want it.
pub(crate) fn query_since(high_water: Option<&str>) -> Option<String> {
    let millis = parse_timestamp_ms(high_water?)? - SAFETY_MARGIN_MS;
    let moment = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)?;
    Some(moment.format("%Y-%m-%d %H:%M").to_string())
}

/// Later of two RFC 3339-ish timestamps, falling back to the new one when unparsable.
pub(crate) fn later(current: Option<&str>, candidate: &str) -> String {
    match (
        current.and_then(parse_timestamp_ms),
        parse_timestamp_ms(candidate),
    ) {
        (Some(old), Some(new)) if old > new => current.unwrap_or(candidate).to_owned(),
        _ => candidate.to_owned(),
    }
}

pub(crate) fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub(crate) fn array_at<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

impl AtlassianReport {
    /// One-line summary for the CLI and logs.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} pages ({} comments) and {} issues new or updated, {} unchanged, {} removed, {} restricted pages skipped",
            self.pages,
            self.comments,
            self.issues,
            self.unchanged,
            self.removed,
            self.restricted_skipped
        );
        if self.truncated {
            text.push_str("; stopped at max_pages/max_issues, run again to continue");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_the_margin_and_formats_for_cql() {
        assert_eq!(
            query_since(Some("2024-05-02T10:30:45.000Z")).as_deref(),
            Some("2024-05-01 10:30")
        );
        assert_eq!(
            query_since(Some("2024-05-02T10:30:45.000+0200")).as_deref(),
            Some("2024-05-01 08:30")
        );
        assert_eq!(query_since(None), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let policy = RetryPolicy::default();
        let delays: Vec<u64> = (0..8).map(|n| policy.backoff(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn credentials_never_debug_print() {
        let shown = format!(
            "{:?}",
            Credentials::Basic {
                email: "a@b.c".into(),
                token: "s3cret".into()
            }
        );
        assert!(!shown.contains("s3cret") && !shown.contains("a@b.c"));
    }

    #[test]
    fn missing_variable_is_named() {
        let config = AtlassianConfig {
            base_url: "https://acme.atlassian.net".into(),
            deployment: AtlassianDeployment::Cloud,
            email_env: Some("ATL_EMAIL".into()),
            token_env: "ATL_TOKEN".into(),
            confluence_spaces: vec!["ENG".into()],
            jira_projects: vec![],
            jira_jql: None,
            out: None,
            interval_minutes: 0,
            include_restricted: false,
            max_pages: 0,
            max_issues: 0,
        };
        let paths = AnchorPaths::new(PathBuf::from("/tmp/x"));
        let only_email = |name: &str| (name == "ATL_EMAIL").then(|| "me@acme.com".to_owned());
        let err = AtlassianOptions::from_config(&config, &paths, &only_email).unwrap_err();
        assert!(err.to_string().contains("ATL_TOKEN"), "{err}");
        let none = |_: &str| None;
        let err = AtlassianOptions::from_config(&config, &paths, &none).unwrap_err();
        assert!(err.to_string().contains("ATL_TOKEN"), "{err}");
    }
}
