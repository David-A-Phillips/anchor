//! Issue boards and discussion boards (GitHub issues and discussions).
//!
//! Input: a directory of JSON files as written by `anchor fetch github` (searched recursively):
//!
//! ```json
//! { "kind": "issue", "repository": "owner/name",
//!   "issue": <GET /repos/{repo}/issues/{n}>, "comments": [<issue comment>, ...] }
//! { "kind": "discussion", "repository": "owner/name",
//!   "discussion": { "number", "title", "body", "url", "createdAt", "updatedAt", "closed",
//!                   "author": {"login"}, "category": {"name"}, "answer": {"id"} | null,
//!                   "comments": { "nodes": [ { "id", "body", "createdAt", "author": {"login"},
//!                                             "replies": { "nodes": [ ... ] } } ] } } }
//! ```
//!
//! Output: one unit per issue or discussion: a `Ticket` node (`issue_type` = `issue` or
//! `discussion`) that tracks the repository's service, its author, its description, links to the
//! entities its text mentions and, when `include_comments` is set, one thread of comments.

use std::path::Path;

use serde_json::Value;

use super::party::PersonIdentity;
use super::{IngestOutput, mentions};
use crate::base::time::parse_timestamp_ms;
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Lookup, NodeAssertion, NodeKey, NodeKind, Result, SourceId,
    Unit, UnitKind, attrs, keys,
};

/// How much of each item to ingest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IssueOptions {
    /// Ingest comment threads, not just the title, state and description.
    pub include_comments: bool,
}

impl Default for IssueOptions {
    fn default() -> Self {
        Self {
            include_comments: true,
        }
    }
}

fn text<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn person(unit: &mut Unit, login: Option<&str>) -> Option<NodeKey> {
    let node = PersonIdentity::new(None, login, None).assertion()?;
    Some(unit.add_node(node))
}

struct Comment<'a> {
    id: String,
    body: &'a str,
    author: Option<&'a str>,
    created: Option<&'a str>,
    reply_to: Option<String>,
}

/// Parses one exported document into a unit. `None` when it is neither an issue nor a discussion.
pub fn parse_issue_document(
    source: &SourceId,
    document: &Value,
    options: IssueOptions,
) -> Option<Unit> {
    let discussion = document.get("discussion").filter(|value| value.is_object());
    let item = discussion.or_else(|| document.get("issue").filter(|value| value.is_object()))?;
    let number = item.get("number").and_then(Value::as_u64)?;
    let repository = text(document, &["repository"]).unwrap_or("unknown/unknown");
    let repo_name = repository.rsplit('/').next().unwrap_or(repository);
    let kind = if discussion.is_some() {
        "discussion"
    } else {
        "issue"
    };
    let local = format!("{repo_name}#{number}");

    let mut unit = Unit::new(
        keys::unit(source, UnitKind::Ticket, &format!("{kind}/{local}")),
        source.clone(),
        UnitKind::Ticket,
    );
    let title = text(item, &["title"]).unwrap_or("(untitled)");
    let status = if discussion.is_some() {
        match (
            item.get("closed").and_then(Value::as_bool),
            item.get("answer").is_some_and(|a| !a.is_null()),
        ) {
            (_, true) => "answered",
            (Some(true), _) => "closed",
            _ => "open",
        }
    } else {
        text(item, &["state"]).unwrap_or("open")
    };
    let labels: Vec<Value> = item
        .get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.get("name").cloned())
                .collect()
        })
        .unwrap_or_default();
    let created = text(item, &["created_at"]).or_else(|| text(item, &["createdAt"]));
    let updated = text(item, &["updated_at"]).or_else(|| text(item, &["updatedAt"]));
    let ticket = unit.add_node(
        NodeAssertion::new(NodeKind::Ticket, keys::ticket(&local), title)
            .alias(local.as_str())
            .alias(format!("{repository}#{number}"))
            .attr(attrs::TITLE, title)
            .attr(attrs::STATUS, status)
            .attr(attrs::ISSUE_TYPE, kind)
            .attr(attrs::LABELS, Value::Array(labels))
            .attr_opt(
                attrs::URL,
                text(item, &["html_url"]).or_else(|| text(item, &["url"])),
            )
            .attr_opt(attrs::CREATED_AT, created.and_then(parse_timestamp_ms))
            .attr_opt(attrs::UPDATED_AT, updated.and_then(parse_timestamp_ms)),
    );
    unit.add_edge(EdgeAssertion::new(
        &ticket,
        EdgeKind::Tracks,
        Lookup::new(&[NodeKind::Service], repo_name),
    ));
    let author = text(item, &["user", "login"]).or_else(|| text(item, &["author", "login"]));
    if let Some(author) = person(&mut unit, author) {
        unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::AuthoredBy, author));
    }
    for assignee in item
        .get("assignees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(assignee) = person(&mut unit, text(assignee, &["login"])) {
            unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::AssignedTo, assignee));
        }
    }
    let body = text(item, &["body"]).unwrap_or("");
    unit.add_chunk(
        &ticket,
        ChunkKind::Description,
        format!("{title}\n\n{body}"),
    );
    unit.edges.extend(mentions::mention_edges(
        &ticket,
        &format!("{title}\n{body}"),
    ));

    if options.include_comments {
        let mut comments = Vec::new();
        if discussion.is_some() {
            for node in item
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = text(node, &["id"]) else {
                    continue;
                };
                comments.push(Comment {
                    id: id.to_owned(),
                    body: text(node, &["body"]).unwrap_or(""),
                    author: text(node, &["author", "login"]),
                    created: text(node, &["createdAt"]),
                    reply_to: None,
                });
                for reply in node
                    .pointer("/replies/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(reply_id) = text(reply, &["id"]) else {
                        continue;
                    };
                    comments.push(Comment {
                        id: reply_id.to_owned(),
                        body: text(reply, &["body"]).unwrap_or(""),
                        author: text(reply, &["author", "login"]),
                        created: text(reply, &["createdAt"]),
                        reply_to: Some(id.to_owned()),
                    });
                }
            }
        } else {
            for node in document
                .get("comments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = node.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                comments.push(Comment {
                    id: id.to_string(),
                    body: text(node, &["body"]).unwrap_or(""),
                    author: text(node, &["user", "login"]),
                    created: text(node, &["created_at"]),
                    reply_to: None,
                });
            }
        }
        if !comments.is_empty() {
            let thread = unit.add_node(
                NodeAssertion::new(
                    NodeKind::Thread,
                    keys::thread(source, &format!("{kind}-{local}")),
                    format!("Discussion on {local}"),
                )
                .attr(attrs::THREAD_KIND, "page")
                .attr(
                    attrs::THREAD_STATUS,
                    if status == "open" {
                        attrs::THREAD_OPEN
                    } else {
                        attrs::THREAD_RESOLVED
                    },
                ),
            );
            unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::Contains, &thread));
            for comment in comments {
                let key = keys::comment(source, &format!("{local}-{}", comment.id));
                let node = unit.add_node(
                    NodeAssertion::new(NodeKind::Comment, key, format!("Comment on {local}"))
                        .attr_opt(
                            attrs::CREATED_AT,
                            comment.created.and_then(parse_timestamp_ms),
                        ),
                );
                unit.add_edge(EdgeAssertion::new(&thread, EdgeKind::Contains, &node));
                if let Some(parent) = &comment.reply_to {
                    unit.add_edge(EdgeAssertion::new(
                        &node,
                        EdgeKind::ReplyTo,
                        keys::comment(source, &format!("{local}-{parent}")),
                    ));
                }
                if let Some(author) = person(&mut unit, comment.author) {
                    unit.add_edge(EdgeAssertion::new(&node, EdgeKind::AuthoredBy, author));
                }
                unit.add_chunk(&node, ChunkKind::Comment, comment.body);
                unit.edges
                    .extend(mentions::mention_edges(&node, comment.body));
            }
        }
    }
    Some(unit.finish())
}

/// Ingests every `*.json` file under `dir` (recursively). Files that are not issues or
/// discussions (for example exported pull requests) are skipped silently; unreadable or malformed
/// files are reported as warnings.
pub fn scan_issues(dir: &Path, source: &SourceId, options: IssueOptions) -> Result<IngestOutput> {
    let mut output = IngestOutput::default();
    if !dir.is_dir() {
        output.warn(dir.display().to_string(), "not a directory");
        return Ok(output);
    }
    let mut files: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "json")
        })
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    for path in files {
        let name = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        match std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|err| err.to_string()))
        {
            Ok(document) => {
                if let Some(unit) = parse_issue_document(source, &document, options) {
                    output.units.push(unit);
                }
            }
            Err(err) => output.warn(name, err),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issue() -> Value {
        json!({ "kind": "issue", "repository": "acme/payments-api",
            "issue": { "number": 12, "title": "Refunds fail for PAY-9", "state": "open", "body": "See `Ledger::charge` and POST /v1/refunds.",
                       "html_url": "https://example.test/12", "created_at": "2026-01-02T03:04:05Z",
                       "user": {"login": "asmith"}, "labels": [{"name": "bug"}], "assignees": [{"login": "bjones"}] },
            "comments": [ { "id": 7, "body": "Blocked on the ledger migration.", "user": {"login": "bjones"}, "created_at": "2026-01-03T00:00:00Z" } ] })
    }

    #[test]
    fn issues_become_tickets_with_threads_and_links() {
        let source = SourceId::new("gh");
        let unit = parse_issue_document(&source, &issue(), IssueOptions::default()).expect("unit");
        let ticket = unit
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Ticket)
            .expect("ticket");
        assert_eq!(ticket.key.as_str(), "ticket:PAYMENTS-API#12");
        assert_eq!(ticket.attrs[attrs::STATUS], "open");
        assert_eq!(ticket.attrs[attrs::ISSUE_TYPE], "issue");
        assert_eq!(
            unit.nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Comment)
                .count(),
            1
        );
        assert_eq!(
            unit.nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Person)
                .count(),
            2
        );
        assert!(unit.edges.iter().any(|e| e.kind == EdgeKind::Tracks));
        assert!(unit.edges.iter().any(|e| e.kind == EdgeKind::AssignedTo));
        assert!(unit.edges.iter().any(|e| e.kind == EdgeKind::Mentions
            && matches!(&e.to, crate::base::NodeRef::Lookup(l) if l.text == "PAY-9")));
        assert_eq!(unit.chunks.len(), 2);

        let shallow = parse_issue_document(
            &source,
            &issue(),
            IssueOptions {
                include_comments: false,
            },
        )
        .expect("unit");
        assert!(
            shallow
                .nodes
                .iter()
                .all(|n| n.kind != NodeKind::Comment && n.kind != NodeKind::Thread)
        );
        assert_eq!(shallow.chunks.len(), 1);
    }

    #[test]
    fn discussions_keep_replies_and_answered_state() {
        let document = json!({ "kind": "discussion", "repository": "acme/payments-api",
            "discussion": { "number": 3, "title": "How do retries work?", "body": "Question.", "closed": false, "answer": {"id": "A1"},
                "author": {"login": "carol"}, "createdAt": "2026-02-01T00:00:00Z",
                "comments": { "nodes": [ { "id": "C1", "body": "They back off.", "author": {"login": "dave"}, "createdAt": "2026-02-02T00:00:00Z",
                    "replies": { "nodes": [ { "id": "R1", "body": "Thanks!", "author": {"login": "carol"}, "createdAt": "2026-02-03T00:00:00Z" } ] } } ] } } });
        let unit = parse_issue_document(&SourceId::new("gh"), &document, IssueOptions::default())
            .expect("unit");
        let ticket = unit
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Ticket)
            .expect("ticket");
        assert_eq!(ticket.attrs[attrs::STATUS], "answered");
        assert_eq!(ticket.attrs[attrs::ISSUE_TYPE], "discussion");
        assert_eq!(
            unit.edges
                .iter()
                .filter(|e| e.kind == EdgeKind::ReplyTo)
                .count(),
            1
        );
        assert!(
            parse_issue_document(
                &SourceId::new("gh"),
                &json!({"pull_request": {}}),
                IssueOptions::default()
            )
            .is_none()
        );
    }
}
