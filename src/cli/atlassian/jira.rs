//! Jira export.
//!
//! Endpoints and fields relied on (unverified against a live site, see `docs/atlassian.md`):
//!
//! * Cloud: `GET /rest/api/3/search/jql?jql&fields&expand=names&maxResults&nextPageToken` ->
//!   `issues[]`, `names`, `nextPageToken`, `isLast`;
//! * Data Center: `GET /rest/api/2/search?jql&fields&expand=names&maxResults&startAt` ->
//!   `issues[]`, `names`, `total`;
//! * `GET {prefix}/issue/{key}/comment?startAt&maxResults` -> `comments[]`, `total` (only when the
//!   search result carries fewer comments than `fields.comment.total`).
//!
//! Search is two-phase: first only `fields=updated` to learn which issues changed, then
//! `fields=*all` for those, in batches, oldest first.

use serde_json::{Map, Value, json};

use super::{
    AtlassianDeployment, Run, array_at, later, query_since, safe_name, str_at, write_atomic,
};
use crate::base::Result;

const LIST_SIZE: usize = 100;
const BATCH: usize = 50;

pub(super) async fn export(run: &mut Run<'_>) -> Result<()> {
    let jql = run.options.jql.as_deref().map(strip_order_by);
    if run.options.projects.is_empty() {
        // `jql` is `Some` here: the caller only comes when one of the two is configured.
        let scope = jql.unwrap_or_default();
        export_scope(run, "jql", &format!("({scope})")).await?;
    } else {
        for project in run.options.projects.clone() {
            let mut scope = format!("project = \"{}\"", project.replace('"', ""));
            if let Some(extra) = &jql {
                scope.push_str(&format!(" AND ({extra})"));
            }
            export_scope(run, &project, &scope).await?;
            if run.report.truncated {
                break;
            }
        }
    }
    Ok(())
}

/// JQL without its `ORDER BY` clause.
fn strip_order_by(jql: &str) -> String {
    let lower = jql.to_ascii_lowercase();
    let cut = lower
        .find(" order by ")
        .or_else(|| lower.starts_with("order by ").then_some(0));
    jql[..cut.unwrap_or(jql.len())].trim().to_owned()
}

async fn export_scope(run: &mut Run<'_>, state_key: &str, scope: &str) -> Result<()> {
    let state = run.state.jira.entry(state_key.to_owned()).or_default();
    let since = query_since(state.high_water.as_deref());
    let jql = match since {
        Some(since) => format!("{scope} AND updated >= \"{since}\" ORDER BY updated ASC"),
        None => format!("{scope} ORDER BY updated ASC"),
    };
    // Phase 1: which issues are new or changed?
    let (listed, _) = search(run, &jql, "updated", false).await?;
    let state = run.state.jira.entry(state_key.to_owned()).or_default();
    let mut changed: Vec<(String, String)> = Vec::new();
    for issue in &listed {
        let Some(key) = issue.get("key").and_then(Value::as_str) else {
            continue;
        };
        let updated = str_at(issue, "/fields/updated").unwrap_or_default();
        if state.issues.get(key).is_some_and(|known| known == updated) {
            run.report.unchanged += 1;
        } else {
            changed.push((key.to_owned(), updated.to_owned()));
        }
    }
    if run.options.max_issues > 0 {
        let remaining = run.options.max_issues.saturating_sub(run.report.issues);
        if changed.len() > remaining {
            changed.truncate(remaining);
            run.report.truncated = true;
        }
    }
    // Phase 2: download them in batches (still oldest first).
    for batch in changed.chunks(BATCH) {
        let keys: Vec<String> = batch.iter().map(|(key, _)| format!("\"{key}\"")).collect();
        let batch_jql = format!("key in ({}) ORDER BY updated ASC", keys.join(","));
        let (issues, names) = search(run, &batch_jql, "*all", true).await?;
        for mut issue in issues {
            let Some(key) = issue.get("key").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            complete_comments(run, &key, &mut issue).await?;
            if let Some(map) = issue.as_object_mut()
                && !names.is_empty()
            {
                map.insert("names".into(), Value::Object(names.clone()));
            }
            let project = key
                .rsplit_once('-')
                .map_or(key.as_str(), |(project, _)| project);
            let path = run
                .options
                .jira_dir()
                .join(safe_name(project))
                .join(format!("{}.json", safe_name(&key)));
            write_atomic(&path, &serde_json::to_vec_pretty(&issue)?)?;
            let updated = str_at(&issue, "/fields/updated")
                .unwrap_or_default()
                .to_owned();
            let state = run.state.jira.entry(state_key.to_owned()).or_default();
            state.high_water = Some(later(state.high_water.as_deref(), &updated));
            state.issues.insert(key, updated);
            run.checkpoint()?;
            run.report.issues += 1;
        }
    }
    Ok(())
}

/// Runs a search to its end; returns the issues and the merged `names` map.
async fn search(
    run: &Run<'_>,
    jql: &str,
    fields: &str,
    names: bool,
) -> Result<(Vec<Value>, Map<String, Value>)> {
    let cloud = run.options.deployment == AtlassianDeployment::Cloud;
    let path = if cloud {
        format!("{}/search/jql", run.options.jira_prefix())
    } else {
        format!("{}/search", run.options.jira_prefix())
    };
    let size = if names { BATCH } else { LIST_SIZE }.to_string();
    let (mut issues, mut merged) = (Vec::new(), Map::new());
    let (mut token, mut start): (Option<String>, usize) = (None, 0);
    loop {
        let start_text = start.to_string();
        let mut params = vec![
            ("jql", jql),
            ("fields", fields),
            ("maxResults", size.as_str()),
        ];
        if names {
            params.push(("expand", "names"));
        }
        match (&token, cloud) {
            (Some(token), true) => params.push(("nextPageToken", token.as_str())),
            (None, false) => params.push(("startAt", start_text.as_str())),
            (Some(_), false) | (None, true) => {}
        }
        let answer = run.api.get(&path, &params).await?;
        let page = array_at(&answer, "/issues");
        issues.extend(page.iter().cloned());
        if let Some(Value::Object(found)) = answer.get("names") {
            merged.extend(found.clone());
        }
        start += page.len();
        if cloud {
            token = answer
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|_| answer.get("isLast").and_then(Value::as_bool) != Some(true))
                .map(str::to_owned);
            if token.is_none() || page.is_empty() {
                break;
            }
        } else {
            let total = answer
                .get("total")
                .and_then(Value::as_u64)
                .map_or(usize::MAX, |total| {
                    usize::try_from(total).unwrap_or(usize::MAX)
                });
            if page.is_empty() || start >= total {
                break;
            }
            token = None;
        }
    }
    Ok((issues, merged))
}

/// Search results may carry only some of an issue's comments: fetch the rest.
async fn complete_comments(run: &Run<'_>, key: &str, issue: &mut Value) -> Result<()> {
    let have = array_at(issue, "/fields/comment/comments").len() as u64;
    let total = issue
        .pointer("/fields/comment/total")
        .and_then(Value::as_u64)
        .unwrap_or(have);
    if total <= have {
        return Ok(());
    }
    let path = format!("{}/issue/{key}/comment", run.options.jira_prefix());
    let (mut all, mut start) = (Vec::new(), 0usize);
    loop {
        let answer = run
            .api
            .get(
                &path,
                &[("startAt", &start.to_string()), ("maxResults", "100")],
            )
            .await?;
        let page = array_at(&answer, "/comments");
        all.extend(page.iter().cloned());
        start += page.len();
        let total = answer.get("total").and_then(Value::as_u64).unwrap_or(0);
        if page.is_empty() || start as u64 >= total {
            break;
        }
    }
    if let Some(comment) = issue.pointer_mut("/fields/comment") {
        *comment = json!({ "comments": all, "total": all.len() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_order_by;

    #[test]
    fn order_by_is_removed() {
        assert_eq!(
            strip_order_by("labels = x ORDER BY created DESC"),
            "labels = x"
        );
        assert_eq!(strip_order_by("order by created"), "");
        assert_eq!(strip_order_by("labels = x"), "labels = x");
    }
}
