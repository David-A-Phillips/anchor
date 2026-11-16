//! Confluence export (REST v1 content API).
//!
//! Endpoints and fields relied on (all unverified against a live site, see `docs/atlassian.md`):
//!
//! * `GET {prefix}/space?type=global&limit&start` -> `results[].key` (only for `"*"`);
//! * `GET {prefix}/content/search?cql&expand&limit&start` -> `results[]`, `_links.next`;
//! * `GET {prefix}/content/{id}?expand=body.storage,version,history,ancestors,space,
//!   metadata.labels,restrictions.read.restrictions.user,restrictions.read.restrictions.group`;
//! * `GET {prefix}/content/{id}/child/comment?expand=body.storage,history,ancestors,
//!   extensions.inlineProperties,extensions.resolution&depth=all&limit&start`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::storage::{escape, storage_to_html, storage_to_text};
use super::{PageEntry, Run, array_at, later, query_since, safe_name, str_at, write_atomic};
use crate::base::{Error, Result};

const PAGE_EXPAND: &str = "body.storage,version,history,ancestors,space,metadata.labels,restrictions.read.restrictions.user,restrictions.read.restrictions.group";
const LIST_EXPAND: &str = "version,ancestors";
const RESTRICTION_EXPAND: &str =
    "version,restrictions.read.restrictions.user,restrictions.read.restrictions.group";
const COMMENT_EXPAND: &str =
    "body.storage,history,ancestors,extensions.inlineProperties,extensions.resolution";
const PAGE_SIZE: usize = 100;

pub(super) async fn export(run: &mut Run<'_>) -> Result<()> {
    let spaces = if run.options.spaces.iter().any(|space| space == "*") {
        all_spaces(run).await?
    } else {
        run.options.spaces.clone()
    };
    let mut pages_written = 0;
    for space in spaces {
        export_space(run, &space, &mut pages_written).await?;
        if run.report.truncated {
            break;
        }
    }
    Ok(())
}

async fn all_spaces(run: &Run<'_>) -> Result<Vec<String>> {
    let path = format!("{}/space", run.options.confluence_prefix());
    let (mut keys, mut start) = (Vec::new(), 0usize);
    loop {
        let limit = PAGE_SIZE.to_string();
        let answer = run
            .api
            .get(
                &path,
                &[
                    ("type", "global"),
                    ("limit", &limit),
                    ("start", &start.to_string()),
                ],
            )
            .await?;
        let results = array_at(&answer, "/results");
        keys.extend(
            results
                .iter()
                .filter_map(|space| space.get("key").and_then(Value::as_str))
                .map(str::to_owned),
        );
        start += results.len();
        if results.is_empty()
            || (results.len() < PAGE_SIZE && answer.pointer("/_links/next").is_none())
        {
            return Ok(keys);
        }
    }
}

/// One page of CQL results and whether more may follow.
async fn search(
    run: &Run<'_>,
    cql: &str,
    expand: &str,
    start: usize,
) -> Result<(Vec<Value>, bool)> {
    let path = format!("{}/content/search", run.options.confluence_prefix());
    let answer = run
        .api
        .get(
            &path,
            &[
                ("cql", cql),
                ("expand", expand),
                ("limit", &PAGE_SIZE.to_string()),
                ("start", &start.to_string()),
            ],
        )
        .await?;
    let results = array_at(&answer, "/results").to_vec();
    let more = answer.pointer("/_links/next").is_some() || results.len() >= PAGE_SIZE;
    Ok((results, more))
}

fn id_of(value: &Value) -> Option<String> {
    match value.get("id")? {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// `user:<name>` / `group:<name>` entries of a page's direct read restrictions.
fn restrictions(page: &Value) -> Vec<String> {
    let mut labels = Vec::new();
    for user in array_at(page, "/restrictions/read/restrictions/user/results") {
        let name = ["displayName", "publicName", "username", "accountId"]
            .iter()
            .find_map(|key| user.get(*key).and_then(Value::as_str));
        if let Some(name) = name {
            labels.push(format!("user:{name}"));
        }
    }
    for group in array_at(page, "/restrictions/read/restrictions/group/results") {
        if let Some(name) = group.get("name").and_then(Value::as_str) {
            labels.push(format!("group:{name}"));
        }
    }
    labels
}

fn ancestor_ids(page: &Value) -> Vec<String> {
    array_at(page, "/ancestors")
        .iter()
        .filter_map(id_of)
        .collect()
}

/// The restrictions on any of `ancestors` (restrictions are inherited by child pages).
fn inherited(ancestors: &[String], own: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    ancestors
        .iter()
        .filter_map(|id| own.get(id))
        .flatten()
        .cloned()
        .collect()
}

fn remove_page_files(run: &Run<'_>, space: &str, id: &str) {
    let dir = run.options.wiki_dir().join(safe_name(space));
    for name in [
        format!("{}.html", safe_name(id)),
        format!("{}.comments.json", safe_name(id)),
    ] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

async fn export_space(run: &mut Run<'_>, space: &str, written: &mut usize) -> Result<()> {
    let include = run.options.include_restricted;
    let quoted = space.replace('"', "");
    let base_cql = format!("space = \"{quoted}\" AND type = page");

    // Pass 1: every page id with its direct restrictions. Needed to notice deletions and pages that
    // became restricted, and to know what an unchanged-listing page inherits.
    let mut all_ids = BTreeSet::new();
    let mut own: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut start = 0;
    loop {
        let (results, more) = search(
            run,
            &format!("{base_cql} ORDER BY lastmodified ASC"),
            RESTRICTION_EXPAND,
            start,
        )
        .await?;
        start += results.len();
        for item in &results {
            if let Some(id) = id_of(item) {
                let labels = restrictions(item);
                if !labels.is_empty() {
                    own.insert(id.clone(), labels);
                }
                all_ids.insert(id);
            }
        }
        if results.is_empty() || !more {
            break;
        }
    }
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    if !include {
        skipped.extend(own.keys().cloned());
    }

    let state = run.state.confluence.entry(space.to_owned()).or_default();
    if include && !state.included_restricted {
        // Pages skipped earlier may lie before the mark: look at everything again.
        state.high_water = None;
    }
    state.included_restricted = include;
    let stale: Vec<String> = state
        .pages
        .iter()
        .filter(|(id, entry)| {
            !all_ids.contains(*id)
                || (!include
                    && (own.contains_key(*id)
                        || entry
                            .ancestors
                            .iter()
                            .any(|ancestor| own.contains_key(ancestor))))
        })
        .map(|(id, _)| id.clone())
        .collect();
    let since = query_since(state.high_water.as_deref());
    for id in &stale {
        state.pages.remove(id);
    }
    for id in &stale {
        remove_page_files(run, space, id);
        run.report.removed += 1;
    }
    if !stale.is_empty() {
        run.checkpoint()?;
    }

    // Pass 2: pages modified since the mark, oldest first, so the mark is also the resume point.
    let cql = match &since {
        Some(since) => {
            format!("{base_cql} AND lastmodified >= \"{since}\" ORDER BY lastmodified ASC")
        }
        None => format!("{base_cql} ORDER BY lastmodified ASC"),
    };
    let mut start = 0;
    'listing: loop {
        let (results, more) = search(run, &cql, LIST_EXPAND, start).await?;
        start += results.len();
        for item in &results {
            let Some(id) = id_of(item) else { continue };
            let ancestors = ancestor_ids(item);
            if !include && (own.contains_key(&id) || !inherited(&ancestors, &own).is_empty()) {
                skipped.insert(id);
                continue;
            }
            let when = str_at(item, "/version/when").unwrap_or_default().to_owned();
            let number = item
                .pointer("/version/number")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let known = run
                .state
                .confluence
                .get(space)
                .and_then(|state| state.pages.get(&id));
            if known.is_some_and(|entry| entry.updated == when && entry.version == number) {
                run.report.unchanged += 1;
                continue;
            }
            if run.options.max_pages > 0 && *written >= run.options.max_pages {
                run.report.truncated = true;
                break 'listing;
            }
            let path = format!("{}/content/{id}", run.options.confluence_prefix());
            let page = match run.api.get(&path, &[("expand", PAGE_EXPAND)]).await {
                Ok(page) => page,
                // Deleted or hidden since it was listed.
                Err(Error::NotFound(_)) => continue,
                Err(err) => return Err(err),
            };
            let mut restricted_to = restrictions(&page);
            let ancestors = ancestor_ids(&page);
            restricted_to.extend(inherited(&ancestors, &own));
            restricted_to.sort();
            restricted_to.dedup();
            if !include && !restricted_to.is_empty() {
                skipped.insert(id);
                continue;
            }
            let threads = comments(run, &id).await?;
            let comment_count: usize = threads
                .iter()
                .map(|thread| array_at(thread, "/comments").len())
                .sum();
            let dir: PathBuf = run.options.wiki_dir().join(safe_name(space));
            let stem = safe_name(&id);
            if threads.is_empty() {
                let _ = std::fs::remove_file(dir.join(format!("{stem}.comments.json")));
            } else {
                write_atomic(
                    &dir.join(format!("{stem}.comments.json")),
                    &serde_json::to_vec_pretty(&json!({ "threads": threads }))?,
                )?;
            }
            write_atomic(
                &dir.join(format!("{stem}.html")),
                render_page(
                    run.options.base_url.as_str(),
                    space,
                    &id,
                    &page,
                    &restricted_to,
                )
                .as_bytes(),
            )?;
            let updated = str_at(&page, "/version/when").unwrap_or(&when).to_owned();
            let state = run.state.confluence.entry(space.to_owned()).or_default();
            state.high_water = Some(later(state.high_water.as_deref(), &updated));
            state.pages.insert(
                id,
                PageEntry {
                    version: page
                        .pointer("/version/number")
                        .and_then(Value::as_u64)
                        .unwrap_or(number),
                    updated,
                    ancestors,
                },
            );
            run.checkpoint()?;
            run.report.pages += 1;
            run.report.comments += comment_count;
            *written += 1;
        }
        if results.is_empty() || !more {
            break;
        }
    }
    run.report.restricted_skipped += skipped.len();
    run.checkpoint()
}

fn person(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let name = ["displayName", "publicName", "username"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))?;
    Some(
        match value
            .get("email")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
        {
            Some(email) => format!("{name} <{email}>"),
            None => name.to_owned(),
        },
    )
}

fn meta(out: &mut String, name: &str, content: &str) {
    if !content.is_empty() {
        out.push_str(&format!(
            "<meta name=\"{name}\" content=\"{}\">\n",
            escape(content)
        ));
    }
}

/// The HTML file of one page; the `<meta>` names are the ones the wiki ingester reads.
fn render_page(
    base_url: &str,
    space: &str,
    id: &str,
    page: &Value,
    restricted_to: &[String],
) -> String {
    let title = str_at(page, "/title").unwrap_or(id);
    let mut out = String::from("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!("<title>{}</title>\n", escape(title)));
    meta(&mut out, "title", title);
    meta(&mut out, "page-id", id);
    if let Some(parent) = array_at(page, "/ancestors").last() {
        meta(&mut out, "parent-id", &id_of(parent).unwrap_or_default());
        meta(
            &mut out,
            "parent-title",
            str_at(parent, "/title").unwrap_or_default(),
        );
    }
    let labels: Vec<&str> = array_at(page, "/metadata/labels/results")
        .iter()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .collect();
    meta(&mut out, "labels", &labels.join(","));
    meta(
        &mut out,
        "author",
        &person(page.pointer("/history/createdBy")).unwrap_or_default(),
    );
    meta(
        &mut out,
        "created",
        str_at(page, "/history/createdDate").unwrap_or_default(),
    );
    meta(
        &mut out,
        "updated",
        str_at(page, "/version/when").unwrap_or_default(),
    );
    let link_base = str_at(page, "/_links/base").unwrap_or(base_url);
    if let Some(webui) = str_at(page, "/_links/webui") {
        meta(&mut out, "url", &format!("{link_base}{webui}"));
    }
    meta(
        &mut out,
        "space",
        str_at(page, "/space/key").unwrap_or(space),
    );
    let version = page
        .pointer("/version/number")
        .map(|number| number.to_string())
        .unwrap_or_default();
    meta(&mut out, "version", &version);
    if !restricted_to.is_empty() {
        meta(
            &mut out,
            "restricted_to",
            &serde_json::to_string(restricted_to).unwrap_or_default(),
        );
    }
    out.push_str("</head>\n<body>\n<main>\n");
    out.push_str(&storage_to_html(
        str_at(page, "/body/storage/value").unwrap_or_default(),
    ));
    out.push_str("\n</main>\n</body>\n</html>\n");
    out
}

/// Footer and inline comments of a page grouped into threads in the wiki sidecar layout.
async fn comments(run: &Run<'_>, id: &str) -> Result<Vec<Value>> {
    let path = format!(
        "{}/content/{id}/child/comment",
        run.options.confluence_prefix()
    );
    let (mut all, mut start) = (Vec::new(), 0usize);
    loop {
        let answer = match run
            .api
            .get(
                &path,
                &[
                    ("expand", COMMENT_EXPAND),
                    ("depth", "all"),
                    ("limit", &PAGE_SIZE.to_string()),
                    ("start", &start.to_string()),
                ],
            )
            .await
        {
            Ok(answer) => answer,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let results = array_at(&answer, "/results");
        all.extend(results.iter().cloned());
        start += results.len();
        if results.is_empty()
            || (answer.pointer("/_links/next").is_none() && results.len() < PAGE_SIZE)
        {
            break;
        }
    }
    Ok(build_threads(&all))
}

fn build_threads(comments: &[Value]) -> Vec<Value> {
    let ids: BTreeSet<String> = comments.iter().filter_map(id_of).collect();
    let mut parent_of: BTreeMap<String, String> = BTreeMap::new();
    for comment in comments {
        let Some(id) = id_of(comment) else { continue };
        let parent = array_at(comment, "/ancestors")
            .iter()
            .rev()
            .filter_map(id_of)
            .find(|ancestor| ancestor != &id && ids.contains(ancestor));
        if let Some(parent) = parent {
            parent_of.insert(id, parent);
        }
    }
    let root_of = |start: &str| -> String {
        let mut current = start.to_owned();
        for _ in 0..64 {
            match parent_of.get(&current) {
                Some(parent) => current = parent.clone(),
                None => break,
            }
        }
        current
    };
    let mut order: Vec<String> = Vec::new();
    let mut members: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for comment in comments {
        let Some(id) = id_of(comment) else { continue };
        let root = root_of(&id);
        if !members.contains_key(&root) {
            order.push(root.clone());
        }
        members.entry(root).or_default().push(comment);
    }
    let by_id: BTreeMap<String, &Value> = comments
        .iter()
        .filter_map(|comment| id_of(comment).map(|id| (id, comment)))
        .collect();
    order
        .iter()
        .map(|root| {
            let head = by_id.get(root).copied().or_else(|| members[root].first().copied());
            let head = head.cloned().unwrap_or(Value::Null);
            let anchor = str_at(&head, "/extensions/inlineProperties/originalSelection");
            let inline = str_at(&head, "/extensions/location") == Some("inline") || anchor.is_some();
            let raw_status = str_at(&head, "/extensions/resolution/status")
                .unwrap_or("open")
                .to_ascii_lowercase();
            let status = match raw_status.as_str() {
                "resolved" | "dismissed" => raw_status.as_str(),
                _ => "open",
            };
            let mut thread = json!({
                "id": root,
                "kind": if inline { "inline" } else { "page" },
                "status": status,
                "comments": members[root].iter().map(|comment| json!({
                    "id": id_of(comment),
                    "author": comment_author(comment),
                    "created": str_at(comment, "/history/createdDate"),
                    "body": storage_to_text(str_at(comment, "/body/storage/value").unwrap_or_default()),
                    "reply_to": id_of(comment).and_then(|id| parent_of.get(&id).cloned()),
                })).collect::<Vec<_>>(),
            });
            if let Some(map) = thread.as_object_mut() {
                if let Some(anchor) = anchor {
                    map.insert("anchor".into(), json!(anchor));
                }
                if status != "open" {
                    if let Some(by) = person(head.pointer("/extensions/resolution/lastModifier")) {
                        map.insert("closed_by".into(), json!(by));
                    }
                    if let Some(at) = str_at(&head, "/extensions/resolution/lastModifiedAt") {
                        map.insert("closed_at".into(), json!(at));
                    }
                }
            }
            thread
        })
        .collect()
}

fn comment_author(comment: &Value) -> Value {
    let Some(user) = comment.pointer("/history/createdBy") else {
        return Value::Null;
    };
    let mut author = serde_json::Map::new();
    if let Some(name) = ["displayName", "publicName"]
        .iter()
        .find_map(|key| user.get(*key).and_then(Value::as_str))
    {
        author.insert("name".into(), json!(name));
    }
    if let Some(handle) = user.get("username").and_then(Value::as_str) {
        author.insert("handle".into(), json!(handle));
    }
    if let Some(email) = user
        .get("email")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        author.insert("email".into(), json!(email));
    }
    Value::Object(author)
}
