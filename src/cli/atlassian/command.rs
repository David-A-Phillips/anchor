//! The `anchor fetch-atlassian` command and the daemon's periodic refresh.

use std::path::PathBuf;
use std::sync::Arc;

use super::{AtlassianOptions, AtlassianReport, Sleeper, TokioSleeper, fetch_atlassian};
use crate::base::{Error, Result};
use crate::config::{AnchorConfig, AtlassianConfig, AtlassianDeployment};
use crate::engine::Engine;

/// Arguments of `anchor fetch-atlassian`. Without `base_url`, every `[[atlassian]]` entry of the
/// configuration is refreshed; with it, the flags describe one site (credentials still come from
/// the named environment variables).
#[derive(Clone, Debug, clap::Args)]
#[allow(rustdoc::invalid_html_tags)] // the field docs are the `--help` text, where `<host>` reads naturally
pub struct FetchAtlassianArgs {
    /// Site URL (`https://acme.atlassian.net`); omit to refresh every `[[atlassian]]` entry.
    pub base_url: Option<String>,
    /// `cloud` or `data_center`.
    #[arg(long, value_enum, default_value = "cloud")]
    pub deployment: AtlassianDeployment,
    /// Cloud: environment variable holding the account e-mail.
    #[arg(long)]
    pub email_env: Option<String>,
    /// Environment variable holding the API token (Cloud) or personal access token (Data Center).
    #[arg(long, default_value = "ATLASSIAN_TOKEN")]
    pub token_env: String,
    /// Confluence space key to export (repeatable; `*` = all global spaces).
    #[arg(long = "space")]
    pub spaces: Vec<String>,
    /// Jira project key to export (repeatable).
    #[arg(long = "project")]
    pub projects: Vec<String>,
    /// Extra JQL.
    #[arg(long)]
    pub jql: Option<String>,
    /// Output directory (default: <data dir>/atlassian/<host>).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Also export pages with read restrictions (recorded as `restricted_to`).
    #[arg(long)]
    pub include_restricted: bool,
    /// Most pages to download in this run (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub max_pages: usize,
    /// Most issues to download in this run (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub max_issues: usize,
}

fn process_env_string(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Runs `anchor fetch-atlassian`: exports, then prints the report and the sources to add.
///
/// # Errors
/// An invocation without a site and without `[[atlassian]]` entries, invalid flags, unset
/// credential variables, and every error of [`fetch_atlassian`].
pub async fn run_command(config: &AnchorConfig, args: FetchAtlassianArgs) -> Result<()> {
    let paths = config.paths();
    let Some(base_url) = args.base_url else {
        if config.atlassian.is_empty() {
            return Err(Error::invalid(
                "name a site (https://acme.atlassian.net --space ENG ...) or add [[atlassian]] entries to the configuration",
            ));
        }
        for entry in &config.atlassian {
            let options = AtlassianOptions::from_config(entry, &paths, &process_env_string)?;
            let report = fetch_atlassian(&options, &TokioSleeper).await?;
            println!("{}: {}.", entry.base_url, report.summary());
        }
        return Ok(());
    };
    let entry = AtlassianConfig {
        base_url: base_url.clone(),
        deployment: args.deployment,
        email_env: args.email_env,
        token_env: args.token_env,
        confluence_spaces: args.spaces,
        jira_projects: args.projects,
        jira_jql: args.jql,
        out: args.out,
        interval_minutes: 0,
        include_restricted: args.include_restricted,
        max_pages: args.max_pages,
        max_issues: args.max_issues,
    };
    AnchorConfig {
        atlassian: vec![entry.clone()],
        ..AnchorConfig::default()
    }
    .validate()
    .into_result()?;
    let options = AtlassianOptions::from_config(&entry, &paths, &process_env_string)?;
    let report = fetch_atlassian(&options, &TokioSleeper).await?;
    println!("Exported from {base_url}: {}.", report.summary());
    let id = crate::base::keys::slugify(&entry.host_slug());
    println!("\nAdd these sources to your configuration, then run `anchor ingest`:\n");
    if !options.spaces.is_empty() {
        println!(
            "[[sources]]\nid = \"{id}-wiki\"\nkind = \"wiki\"\ntrust = \"reference_only\"\nlocation = \"{}\"\n",
            options.wiki_dir().display()
        );
    }
    if !options.projects.is_empty() || options.jql.is_some() {
        println!(
            "[[sources]]\nid = \"{id}-jira\"\nkind = \"jira\"\ntrust = \"reference_only\"\nlocation = \"{}\"\n",
            options.jira_dir().display()
        );
    }
    let deployment = match entry.deployment {
        AtlassianDeployment::Cloud => "cloud",
        AtlassianDeployment::DataCenter => "data_center",
    };
    println!(
        "To keep it refreshed by the daemon, also add an [[atlassian]] entry (see docs/atlassian.md):\n\n[[atlassian]]\nbase_url = \"{base_url}\"\ndeployment = \"{deployment}\"\ntoken_env = \"{}\"\ninterval_minutes = 60",
        entry.token_env
    );
    Ok(())
}

/// One refresh of a configured site: exports what changed and, when anything did, re-scans the
/// sources reading the export directory (so the graph follows the files).
pub async fn refresh_once(
    engine: &Arc<Engine>,
    options: &AtlassianOptions,
    sleeper: &dyn Sleeper,
) -> Result<AtlassianReport> {
    let report = fetch_atlassian(options, sleeper).await?;
    if report.pages + report.issues + report.removed > 0 {
        crate::cli::daemon::rescan_under(engine, &options.out).await?;
    }
    Ok(report)
}

/// Starts the daemon's periodic refresh of every `[[atlassian]]` entry with `interval_minutes > 0`.
/// After a refresh that changed files, the sources reading the export directory are re-scanned.
pub fn spawn_refreshers(engine: &Arc<Engine>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    for entry in engine
        .config()
        .atlassian
        .iter()
        .filter(|entry| entry.interval_minutes > 0)
    {
        let options =
            match AtlassianOptions::from_config(entry, engine.paths(), &process_env_string) {
                Ok(options) => options,
                Err(err) => {
                    tracing::warn!("not refreshing {}: {err}", entry.base_url);
                    continue;
                }
            };
        let period = crate::cli::daemon::refresh_period(entry.interval_minutes);
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(period);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                timer.tick().await;
                match refresh_once(&engine, &options, &TokioSleeper).await {
                    Ok(report) => {
                        tracing::info!("refreshed {}: {}", options.base_url, report.summary());
                    }
                    Err(err) => tracing::warn!("refreshing {} failed: {err}", options.base_url),
                }
            }
        }));
    }
    handles
}
