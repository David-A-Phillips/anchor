//! Jira exports: issue JSON payloads with links, comments and blocking information.
//!
//! # Accepted input
//!
//! A directory (scanned recursively, hidden entries skipped) of `*.json` files. Each file holds one
//! of:
//!
//! * a single issue in Jira REST shape (the response of `GET /rest/api/2/issue/KEY`);
//! * a search result: `{ "issues": [issue, ...], "names": { "customfield_1": "Blocked Reason" } }`;
//! * a JSON array of issues.
//!
//! An issue is:
//!
//! ```json
//! {
//!   "key": "PAY-123",
//!   "self": "https://jira.example.com/rest/api/2/issue/10001",
//!   "names": { "customfield_10050": "Blocked Reason" },
//!   "fields": {
//!     "summary": "...",
//!     "description": "wiki markup string, or an Atlassian Document Format object",
//!     "status": { "name": "Blocked" }, "resolution": { "name": "Done" },
//!     "priority": { "name": "High" }, "issuetype": { "name": "Bug" },
//!     "labels": ["a"], "components": [{ "name": "ledger" }], "fixVersions": [{ "name": "1.2" }],
//!     "assignee": { "displayName": "...", "emailAddress": "...", "name": "...", "accountId": "..." },
//!     "reporter": { }, "creator": { },
//!     "created": "2026-01-05T10:00:00.000+0000", "updated": "...", "duedate": "2026-02-01",
//!     "parent": { "key": "PAY-100" }, "subtasks": [{ "key": "PAY-124" }],
//!     "issuelinks": [{
//!       "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
//!       "inwardIssue": { "key": "PAY-7", "fields": { "summary": "..." } }
//!     }],
//!     "comment": { "comments": [{ "id": "1", "author": { }, "body": "...", "created": "...", "updated": "..." }] },
//!     "customfield_10050": "waiting for the vendor"
//!   }
//! }
//! ```
//!
//! Only `key` is required. Unknown fields are ignored; fields of an unexpected type are treated as
//! absent. Wherever Jira uses `{ "name": ... }` objects a plain string is accepted too. The optional
//! `names` map (`?expand=names`) gives custom fields their display names.
//!
//! # Output
//!
//! One [`Unit`] of kind [`UnitKind::Ticket`](crate::base::UnitKind::Ticket) per issue, with key
//! `keys::unit(source, UnitKind::Ticket, KEY)`; see [`parse_issue`] for what it asserts.
//!
//! # Link direction
//!
//! A link entry carries either `outwardIssue` (this issue *outward-phrase* the other one) or
//! `inwardIssue` (this issue *inward-phrase* the other one). The phrase decides who the subject
//! is: a passive phrase ("is blocked by", "is superseded by", "is required for") makes the other
//! issue the subject. Without a phrase, `outwardIssue` means this issue is the subject.
//!
//! All functions here block (file I/O and parsing): call them from a blocking thread.

mod issue;
mod text;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::Path;

use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::IngestOutput;
use crate::base::{Error, Result, SourceId, Unit, attrs};

pub use text::{adf_to_text, markup_to_text};

/// Tuning of Jira ingestion. `Default` suits a stock Jira workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JiraOptions {
    /// Base URL of the Jira site (`https://jira.example.com`), used for the `url` attribute
    /// (`<base>/browse/<KEY>`). When `None` the URL is derived from the issue's `self` link.
    pub base_url: Option<String>,
    /// Status names (compared case-insensitively) that mean the issue is blocked.
    pub blocked_statuses: Vec<String>,
    /// Extra custom fields (ids such as `customfield_10050`, or display names) whose non-empty
    /// value marks the issue as blocked and states the reason. Fields whose name looks like
    /// "Blocked Reason", "Blocked", "Flagged" or "Impediment" are recognised without being listed.
    pub blocked_fields: Vec<String>,
    /// Files larger than this are skipped with a warning.
    pub max_file_bytes: u64,
}

impl Default for JiraOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            blocked_statuses: vec!["blocked".to_owned(), "impeded".to_owned()],
            blocked_fields: Vec::new(),
            max_file_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Ingests every `*.json` file under `dir`.
///
/// Malformed files and issues become warnings; only an unusable `dir` is an error
/// ([`Error::NotFound`]). When several payloads describe the same issue the most recently
/// `updated` one wins (the first one on a tie) and a warning is recorded. Units are returned in
/// canonical form, ordered by key, so the same directory always yields the same output.
pub fn scan_jira(dir: &Path, source: &SourceId, options: &JiraOptions) -> Result<IngestOutput> {
    if !dir.is_dir() {
        return Err(Error::not_found(format!(
            "Jira export directory `{}` does not exist or is not a directory",
            dir.display()
        )));
    }
    let mut out = IngestOutput::default();
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                out.warn(dir.display().to_string(), format!("cannot read: {err}"));
                continue;
            }
        };
        let path = entry.path();
        let is_json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !entry.file_type().is_file() || !is_json {
            continue;
        }
        let location = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        match entry.metadata() {
            Ok(meta) if meta.len() > options.max_file_bytes => {
                out.warn(location, "file is too large, skipped");
                continue;
            }
            Ok(_) => {}
            Err(err) => {
                out.warn(location, format!("cannot read: {err}"));
                continue;
            }
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                out.warn(location, format!("cannot read: {err}"));
                continue;
            }
        };
        let parsed = parse_jira_document(source, &location, &text, options);
        out.warnings.extend(parsed.warnings);
        for unit in parsed.units {
            match units.entry(unit.key.as_str().to_owned()) {
                Entry::Vacant(slot) => {
                    slot.insert(unit);
                }
                Entry::Occupied(mut slot) => {
                    out.warn(
                        location.clone(),
                        format!(
                            "issue `{}` appears more than once; keeping the most recently updated payload",
                            slot.key()
                        ),
                    );
                    if updated_at(&unit) > updated_at(slot.get()) {
                        slot.insert(unit);
                    }
                }
            }
        }
    }
    out.units = units.into_values().collect();
    Ok(out)
}

fn updated_at(unit: &Unit) -> Option<i64> {
    unit.nodes
        .iter()
        .find_map(|node| node.attrs.get(attrs::UPDATED_AT).and_then(Value::as_i64))
}

/// Parses the text of one export file: a single issue, a search result or an array of issues.
///
/// `location` only labels warnings. Never fails: unparsable text or issues are reported as
/// warnings and the remaining issues are still returned.
pub fn parse_jira_document(
    source: &SourceId,
    location: &str,
    text: &str,
    options: &JiraOptions,
) -> IngestOutput {
    let mut out = IngestOutput::default();
    let document: Value = match serde_json::from_str(text) {
        Ok(document) => document,
        Err(err) => {
            out.warn(location, format!("invalid JSON: {err}"));
            return out;
        }
    };
    let (issues, names): (&[Value], Option<&Map<String, Value>>) = match &document {
        Value::Array(items) => (items, None),
        Value::Object(object) => match object.get("issues") {
            Some(Value::Array(items)) => (items, object.get("names").and_then(Value::as_object)),
            Some(_) => {
                out.warn(location, "`issues` is not an array");
                return out;
            }
            None => (std::slice::from_ref(&document), None),
        },
        _ => {
            out.warn(
                location,
                "expected an issue object, a search result or an array",
            );
            return out;
        }
    };
    for (index, item) in issues.iter().enumerate() {
        match issue::build_issue(source, item, names, options) {
            Ok(unit) => out.units.push(unit),
            Err(err) => out.warn(format!("{location}#{index}"), err.to_string()),
        }
    }
    out
}

/// Builds the unit of one issue (the per-issue entry point).
///
/// The unit asserts:
///
/// * the Ticket node `keys::ticket(KEY)` named `"KEY: summary"`, alias `KEY`, attributes `title`,
///   `status` (lower-cased), `resolution`, `priority`, `issue_type`, `labels`, `created_at`,
///   `updated_at`, `url`, `due_date`, `fix_versions`, and `blocking = true` with `reason` when
///   the status, a flag or a custom field marks the issue as blocked;
/// * `AssignedTo` (assignee) and `AuthoredBy` (reporter, else creator) edges to asserted people;
/// * `parent --Contains--> ticket` for `parent` and an "Epic Link" custom field, and
///   `ticket --Contains--> subtask`;
/// * issue links between ticket keys: `Blocks` (blocker to blocked, with `reason`), `DependsOn`,
///   `Supersedes`, and `RelatesTo` for everything else, each with a `link_type` attribute;
/// * `Tracks` edges to components, and to services, components, APIs and endpoints mentioned in
///   the summary (weight 0.6); `Mentions` edges from the description and the comments;
/// * the description as a Description chunk, and the comments as one page-level Thread
///   (`keys::thread(source, KEY)`, `resolved` once the issue has a resolution) containing one
///   Comment node (`keys::comment(source, "KEY-<comment id>")`) per non-empty comment.
///
/// # Errors
///
/// [`Error::Parse`] when `issue` is not an object or has no usable `key`. Everything else is
/// optional.
pub fn parse_issue(source: &SourceId, issue: &Value, options: &JiraOptions) -> Result<Unit> {
    issue::build_issue(source, issue, None, options)
}

#[cfg(test)]
mod tests;
