//! `anchor fetch github`: exports a repository's issue board, discussion board and pull-request
//! reviews into the JSON layouts the `issues` and `code_review` ingesters read.
//!
//! Uses the GitHub CLI (`gh`), so authentication, enterprise hosts and rate limits follow the
//! user's existing `gh` setup and no token passes through Anchor. Already exported items are
//! skipped, so a fetch can be interrupted and resumed.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::base::{Error, Result};

/// What to export.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    /// `owner/name`.
    pub repository: String,
    /// Output directory; `issues/`, `discussions/` and `pulls/` are created inside it.
    pub out: PathBuf,
    /// Most recently updated issues to export.
    pub issues: usize,
    /// Most recently updated discussions to export.
    pub discussions: usize,
    /// Most recently updated pull requests to export.
    pub pulls: usize,
}

/// What was exported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// New issue files.
    pub issues: usize,
    /// New discussion files.
    pub discussions: usize,
    /// New pull-request files.
    pub pulls: usize,
    /// Items that were already on disk.
    pub skipped: usize,
}

fn gh(args: &[&str]) -> Result<Value> {
    let output = Command::new("gh")
        .arg("api")
        .args(args)
        .output()
        .map_err(|err| {
            Error::other(format!(
                "cannot run the GitHub CLI (`gh`): {err}. Install it and run `gh auth login`."
            ))
        })?;
    if !output.status.success() {
        return Err(Error::other(format!(
            "gh api {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// All pages of a list endpoint, up to `limit` items.
fn list(path: &str, limit: usize) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut page = 1;
    while items.len() < limit {
        let separator = if path.contains('?') { '&' } else { '?' };
        let batch = gh(&[&format!("{path}{separator}per_page=100&page={page}")])?;
        let Some(batch) = batch.as_array().filter(|batch| !batch.is_empty()) else {
            break;
        };
        items.extend(batch.iter().cloned());
        if batch.len() < 100 {
            break;
        }
        page += 1;
    }
    items.truncate(limit);
    Ok(items)
}

/// Writes `document` atomically unless the file exists. Returns whether it was written.
fn save(dir: &Path, number: u64, document: &Value) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(format!("{number}.json"));
    let partial = dir.join(format!("{number}.json.part"));
    std::fs::write(&partial, serde_json::to_vec_pretty(document)?)?;
    std::fs::rename(partial, target)?;
    Ok(())
}

fn exists(dir: &Path, number: u64) -> bool {
    dir.join(format!("{number}.json")).exists()
}

const DISCUSSIONS: &str = "query($owner: String!, $name: String!, $count: Int!) { repository(owner: $owner, name: $name) { discussions(first: $count, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { number title body url createdAt updatedAt closed author { login } category { name } answer { id } comments(first: 50) { nodes { id body createdAt author { login } replies(first: 30) { nodes { id body createdAt author { login } } } } } } } } }";

const THREADS: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviewThreads(first: 100) { nodes { id isResolved isOutdated resolvedBy { login } comments(first: 100) { nodes { databaseId } } } } } } }";

/// Exports issues, discussions and pull requests of one repository.
pub fn fetch_github(options: &FetchOptions) -> Result<FetchReport> {
    let repo = options.repository.trim();
    let Some((owner, name)) = repo
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
    else {
        return Err(Error::invalid(format!(
            "`{repo}` is not of the form owner/name"
        )));
    };
    let mut report = FetchReport::default();

    if options.issues > 0 {
        let dir = options.out.join("issues");
        // The issues endpoint also returns pull requests; over-fetch and filter them out.
        let listed = list(
            &format!("repos/{repo}/issues?state=all&sort=updated&direction=desc"),
            options.issues * 3,
        )?;
        for issue in listed
            .iter()
            .filter(|item| item.get("pull_request").is_none())
            .take(options.issues)
        {
            let Some(number) = issue.get("number").and_then(Value::as_u64) else {
                continue;
            };
            if exists(&dir, number) {
                report.skipped += 1;
                continue;
            }
            let comments = list(&format!("repos/{repo}/issues/{number}/comments"), 500)?;
            save(
                &dir,
                number,
                &json!({ "kind": "issue", "repository": repo, "issue": issue, "comments": comments }),
            )?;
            report.issues += 1;
        }
    }

    if options.discussions > 0 {
        let dir = options.out.join("discussions");
        let count = options.discussions.min(100).to_string();
        // Repositories without a discussion board answer with an error; that is not a failure.
        match gh(&[
            "graphql",
            "-f",
            &format!("query={DISCUSSIONS}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-F",
            &format!("count={count}"),
        ]) {
            Ok(data) => {
                for discussion in data
                    .pointer("/data/repository/discussions/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(number) = discussion.get("number").and_then(Value::as_u64) else {
                        continue;
                    };
                    if exists(&dir, number) {
                        report.skipped += 1;
                        continue;
                    }
                    save(
                        &dir,
                        number,
                        &json!({ "kind": "discussion", "repository": repo, "discussion": discussion }),
                    )?;
                    report.discussions += 1;
                }
            }
            Err(err) => tracing::warn!("discussions of {repo} are not available: {err}"),
        }
    }

    if options.pulls > 0 {
        let dir = options.out.join("pulls");
        for pull in list(
            &format!("repos/{repo}/pulls?state=all&sort=updated&direction=desc"),
            options.pulls,
        )? {
            let Some(number) = pull.get("number").and_then(Value::as_u64) else {
                continue;
            };
            if exists(&dir, number) {
                report.skipped += 1;
                continue;
            }
            let threads = gh(&[
                "graphql",
                "-f",
                &format!("query={THREADS}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-F",
                &format!("number={number}"),
            ])?;
            let threads: Vec<Value> = threads
                .pointer("/data/repository/pullRequest/reviewThreads/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|thread| {
                    json!({
                        "id": thread.get("id"),
                        "is_resolved": thread.get("isResolved"),
                        "is_outdated": thread.get("isOutdated"),
                        "resolved_by": thread.get("resolvedBy"),
                        "comments": thread.pointer("/comments/nodes").and_then(Value::as_array)
                            .map(|nodes| nodes.iter().filter_map(|node| node.get("databaseId").cloned()).collect::<Vec<_>>())
                            .unwrap_or_default(),
                    })
                })
                .collect();
            save(
                &dir,
                number,
                &json!({
                    "pull_request": gh(&[&format!("repos/{repo}/pulls/{number}")])?,
                    "reviews": list(&format!("repos/{repo}/pulls/{number}/reviews"), 500)?,
                    "review_comments": list(&format!("repos/{repo}/pulls/{number}/comments"), 1000)?,
                    "issue_comments": list(&format!("repos/{repo}/issues/{number}/comments"), 500)?,
                    "review_threads": threads,
                }),
            )?;
            report.pulls += 1;
        }
    }
    Ok(report)
}
