//! One Jira issue to one ticket unit.

use serde_json::{Map, Value};

use super::JiraOptions;
use super::text::{adf_to_text, rich_text};
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, Lookup, NodeAssertion, NodeKey, NodeKind, Result,
    SourceId, Unit, UnitKind, attrs, keys,
};
use crate::engine::ingest::mentions::extract_mentions;
use crate::engine::ingest::party::PersonIdentity;

/// Weight of `Tracks` edges inferred from entity names in the summary.
const SUMMARY_TRACKS_WEIGHT: f32 = 0.6;
/// Kinds that a ticket can track when its summary names them.
const TRACKABLE: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::ApiEndpoint,
];
/// Edge attribute holding the Jira link type name ("Blocks", "Duplicate", ...).
const LINK_TYPE: &str = "link_type";

static NULL: Value = Value::Null;

fn nonblank(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str).and_then(nonblank)
}

/// Jira wraps most enumerations as `{ "name": ... }`; exports sometimes flatten them to strings.
fn named(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::String(text) => nonblank(text),
        object @ Value::Object(_) => ["name", "value", "displayName"]
            .iter()
            .find_map(|field| string(object, field)),
        _ => None,
    }
}

fn names_of(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| named(Some(item)))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(text) => crate::base::time::parse_timestamp_ms(text),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Tracker keys are short identifiers; anything else would corrupt node and unit keys.
fn issue_key(value: Option<&Value>) -> Option<String> {
    let key = nonblank(value?.as_str()?)?;
    let valid = key.len() <= 64
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    valid.then(|| key.to_ascii_uppercase())
}

/// Asserts the person described by a Jira user object (or a plain string) and returns its key.
fn person(unit: &mut Unit, value: Option<&Value>) -> Option<(NodeKey, String)> {
    let (identity, account_id) = match value? {
        Value::String(text) => (PersonIdentity::parse(text), None),
        user @ Value::Object(_) => (
            PersonIdentity::new(
                string(user, "displayName"),
                string(user, "name"),
                string(user, "emailAddress"),
            ),
            string(user, "accountId"),
        ),
        _ => return None,
    };
    let mut node = identity.assertion()?;
    // Cloud payloads identify users only by an opaque account id: registering it lets payloads
    // that lack the e-mail (privacy settings) still resolve to the same person.
    if let Some(account_id) = account_id {
        node = node.alias(account_id);
    }
    let name = node.name.clone();
    Some((unit.add_node(node), name))
}

/// What a "blocked"-style field value says.
#[derive(Default)]
struct Flag {
    blocked: bool,
    reason: Option<String>,
}

fn flag_from_text(text: &str) -> Flag {
    let text = text.trim();
    match text.to_lowercase().as_str() {
        "" | "no" | "false" | "none" | "n/a" | "not blocked" => Flag::default(),
        "yes" | "true" | "blocked" | "impediment" | "flagged" => Flag {
            blocked: true,
            reason: None,
        },
        _ => Flag {
            blocked: true,
            reason: Some(text.to_owned()),
        },
    }
}

fn flag_value(value: &Value) -> Flag {
    match value {
        Value::Null => Flag::default(),
        Value::Bool(blocked) => Flag {
            blocked: *blocked,
            reason: None,
        },
        Value::Number(number) => Flag {
            blocked: number.as_f64().is_some_and(|n| n != 0.0),
            reason: None,
        },
        Value::String(text) => flag_from_text(text),
        Value::Array(items) => {
            let flags: Vec<Flag> = items.iter().map(flag_value).collect();
            let reasons: Vec<String> = flags.iter().filter_map(|f| f.reason.clone()).collect();
            Flag {
                blocked: flags.iter().any(|flag| flag.blocked),
                reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
            }
        }
        object @ Value::Object(_) => {
            if string(object, "type") == Some("doc") {
                flag_from_text(&adf_to_text(object))
            } else {
                named(Some(object)).map(flag_from_text).unwrap_or_default()
            }
        }
    }
}

fn is_blocked_field(id: &str, label: &str, options: &JiraOptions) -> bool {
    let id = keys::slugify(id);
    let label = keys::slugify(label);
    if options.blocked_fields.iter().any(|field| {
        let field = keys::slugify(field);
        field == id || field == label
    }) {
        return true;
    }
    matches!(label.as_str(), "blocked" | "flagged")
        || [
            "blocked-reason",
            "blocking-reason",
            "block-reason",
            "blocker-reason",
            "reason-blocked",
            "reason-for-block",
            "impediment",
        ]
        .iter()
        .any(|pattern| label.contains(pattern))
}

/// Scans the custom fields for a blocked flag or reason.
fn blocked_fields(
    fields: &Value,
    names: Option<&Map<String, Value>>,
    options: &JiraOptions,
) -> Flag {
    let mut result = Flag::default();
    let Some(map) = fields.as_object() else {
        return result;
    };
    // Sorted so that the reason picked does not depend on the payload's field order.
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (id, value) in entries {
        let label = names
            .and_then(|names| names.get(id))
            .and_then(Value::as_str)
            .unwrap_or(id);
        if !is_blocked_field(id, label, options) {
            continue;
        }
        let flag = flag_value(value);
        result.blocked |= flag.blocked;
        if result.reason.is_none() {
            result.reason = flag.reason;
        }
    }
    result
}

/// `true` when a link phrase makes the *other* issue the subject ("is blocked by").
fn is_passive(phrase: &str) -> bool {
    let phrase = phrase.trim().to_lowercase();
    phrase.starts_with("is ") || phrase.starts_with("was ") || phrase.ends_with(" by")
}

fn link_kind(description: &str) -> EdgeKind {
    let has = |needles: &[&str]| needles.iter().any(|needle| description.contains(needle));
    if has(&["block"]) {
        EdgeKind::Blocks
    } else if has(&["depend", "require"]) {
        EdgeKind::DependsOn
    } else if has(&["supersede", "replace", "obsolete"]) {
        EdgeKind::Supersedes
    } else {
        EdgeKind::RelatesTo
    }
}

fn browse_url(issue: &Value, key: &str, options: &JiraOptions) -> Option<String> {
    let base = match options.base_url.as_deref().and_then(nonblank) {
        Some(base) => base,
        None => {
            let link = string(issue, "self")?;
            &link[..link.find("/rest/")?]
        }
    };
    Some(format!("{}/browse/{key}", base.trim_end_matches('/')))
}

/// Edges from `from` for the entities named in `text`. In a summary, services, components and
/// APIs are what the ticket is about (`Tracks`); everywhere else they are plain mentions.
fn mention_edges(unit: &mut Unit, from: &NodeKey, own_key: &str, text: &str, summary: bool) {
    for mention in extract_mentions(text) {
        let lookup = mention.lookup;
        if lookup.kinds == [NodeKind::Ticket] && lookup.text.eq_ignore_ascii_case(own_key) {
            continue;
        }
        let edge = if summary && lookup.kinds.iter().any(|kind| TRACKABLE.contains(kind)) {
            EdgeAssertion::new(from, EdgeKind::Tracks, lookup).weight(SUMMARY_TRACKS_WEIGHT)
        } else {
            EdgeAssertion::new(from, EdgeKind::Mentions, lookup).weight(mention.weight)
        };
        unit.add_edge(edge);
    }
}

pub(super) fn build_issue(
    source: &SourceId,
    issue: &Value,
    outer_names: Option<&Map<String, Value>>,
    options: &JiraOptions,
) -> Result<Unit> {
    if !issue.is_object() {
        return Err(Error::parse("issue is not a JSON object"));
    }
    let key = issue_key(issue.get("key")).ok_or_else(|| {
        Error::parse("issue has no valid `key` (expected something like PAY-123)")
    })?;
    let fields = issue
        .get("fields")
        .filter(|fields| fields.is_object())
        .unwrap_or(&NULL);
    let names = issue
        .get("names")
        .and_then(Value::as_object)
        .or(outer_names);
    let field = |name: &str| fields.get(name);

    let ticket = keys::ticket(&key);
    let mut unit = Unit::new(
        keys::unit(source, UnitKind::Ticket, &key),
        source.clone(),
        UnitKind::Ticket,
    );

    let summary = string(fields, "summary");
    let status = named(field("status")).map(str::to_lowercase);
    let resolution = named(field("resolution"));
    let custom = blocked_fields(fields, names, options);
    let status_blocked = status.as_deref().is_some_and(|status| {
        options
            .blocked_statuses
            .iter()
            .any(|blocked| blocked.trim().eq_ignore_ascii_case(status))
    });

    // --- links -------------------------------------------------------------------------------
    let mut blockers: Vec<String> = Vec::new();
    for link in field("issuelinks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let link_type = link.get("type").unwrap_or(&NULL);
        let type_name = named(Some(link_type));
        let (other, phrase, outward) = match (link.get("outwardIssue"), link.get("inwardIssue")) {
            (Some(other @ Value::Object(_)), _) => (other, string(link_type, "outward"), true),
            (_, Some(other @ Value::Object(_))) => (other, string(link_type, "inward"), false),
            _ => continue,
        };
        let Some(other_key) = issue_key(other.get("key")) else {
            continue;
        };
        if other_key == key {
            continue;
        }
        let other_summary = other
            .get("fields")
            .and_then(|fields| string(fields, "summary"));
        let other_ticket = keys::ticket(&other_key);
        let description =
            format!("{} {}", type_name.unwrap_or(""), phrase.unwrap_or("")).to_lowercase();
        let kind = link_kind(&description);
        let this_is_subject = match phrase {
            Some(phrase) => !is_passive(phrase),
            None => outward,
        };
        // Symmetric relations have no meaningful direction: always assert them from this ticket
        // so that both payloads of a pair do not produce two opposite edges per side.
        let mut edge = if this_is_subject || kind == EdgeKind::RelatesTo {
            EdgeAssertion::new(&ticket, kind, &other_ticket)
        } else {
            EdgeAssertion::new(&other_ticket, kind, &ticket)
        };
        if let Some(type_name) = type_name {
            edge = edge.attr(LINK_TYPE, type_name);
        }
        if kind == EdgeKind::Blocks {
            let reason = if this_is_subject {
                summary.map(str::to_owned)
            } else {
                blockers.push(match other_summary {
                    Some(text) => format!("{other_key} ({text})"),
                    None => other_key.clone(),
                });
                custom
                    .reason
                    .clone()
                    .or_else(|| other_summary.map(str::to_owned))
            };
            if let Some(reason) = reason {
                edge = edge.attr(attrs::REASON, reason);
            }
        }
        unit.add_edge(edge);
    }

    // --- the ticket node ---------------------------------------------------------------------
    let name = match summary {
        Some(summary) => format!("{key}: {summary}"),
        None => key.clone(),
    };
    let labels = names_of(field("labels"));
    let fix_versions = names_of(field("fixVersions"));
    let mut node = NodeAssertion::new(NodeKind::Ticket, ticket.clone(), name)
        .alias(key.as_str())
        .attr_opt(attrs::TITLE, summary)
        .attr_opt(attrs::STATUS, status.clone())
        .attr_opt(attrs::RESOLUTION, resolution)
        .attr_opt(attrs::PRIORITY, named(field("priority")))
        .attr_opt(
            attrs::ISSUE_TYPE,
            named(field("issuetype")).map(str::to_lowercase),
        )
        .attr_opt(attrs::CREATED_AT, timestamp(field("created")))
        .attr_opt(attrs::UPDATED_AT, timestamp(field("updated")))
        .attr_opt(attrs::URL, browse_url(issue, &key, options))
        .attr_opt("due_date", string(fields, "duedate"));
    if !labels.is_empty() {
        node = node.attr(attrs::LABELS, labels);
    }
    if !fix_versions.is_empty() {
        node = node.attr("fix_versions", fix_versions);
    }
    if status_blocked || custom.blocked {
        let reason = custom.reason.or_else(|| {
            if blockers.is_empty() {
                status
                    .as_deref()
                    .filter(|_| status_blocked)
                    .map(|status| format!("status is {status}"))
            } else {
                Some(format!("blocked by {}", blockers.join("; ")))
            }
        });
        node = node
            .attr(attrs::BLOCKING, true)
            .attr_opt(attrs::REASON, reason);
    }
    unit.add_node(node);

    // --- people ------------------------------------------------------------------------------
    if let Some((assignee, _)) = person(&mut unit, field("assignee")) {
        unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::AssignedTo, assignee));
    }
    let reporter =
        person(&mut unit, field("reporter")).or_else(|| person(&mut unit, field("creator")));
    if let Some((reporter, _)) = reporter {
        unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::AuthoredBy, reporter));
    }

    // --- hierarchy ---------------------------------------------------------------------------
    let mut parents: Vec<String> = Vec::new();
    parents.extend(field("parent").and_then(|parent| issue_key(parent.get("key"))));
    if let Some(map) = fields.as_object() {
        for (id, value) in map {
            let label = names
                .and_then(|names| names.get(id))
                .and_then(Value::as_str);
            if label.is_some_and(|label| keys::slugify(label) == "epic-link") {
                parents.extend(issue_key(Some(value)));
            }
        }
    }
    for parent in parents.iter().filter(|parent| **parent != key) {
        unit.add_edge(EdgeAssertion::new(
            keys::ticket(parent),
            EdgeKind::Contains,
            &ticket,
        ));
    }
    for subtask in field("subtasks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(child) = issue_key(subtask.get("key")).filter(|child| *child != key) {
            unit.add_edge(EdgeAssertion::new(
                &ticket,
                EdgeKind::Contains,
                keys::ticket(&child),
            ));
        }
    }

    // --- what the ticket is about ------------------------------------------------------------
    for component in names_of(field("components")) {
        unit.add_edge(EdgeAssertion::new(
            &ticket,
            EdgeKind::Tracks,
            Lookup::new(&[NodeKind::Component, NodeKind::Service], component),
        ));
    }
    if let Some(summary) = summary {
        mention_edges(&mut unit, &ticket, &key, summary, true);
    }
    if let Some(description) = rich_text(field("description")) {
        mention_edges(&mut unit, &ticket, &key, &description, false);
        unit.add_chunk(&ticket, ChunkKind::Description, description);
    }

    // --- comments ----------------------------------------------------------------------------
    let comments = match field("comment") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(wrapper @ Value::Object(_)) => wrapper
            .get("comments")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice),
        _ => &[],
    };
    let thread = keys::thread(source, &key);
    let mut has_comments = false;
    for (index, comment) in comments.iter().enumerate() {
        let Some(body) = rich_text(comment.get("body")) else {
            continue;
        };
        let id = match comment.get("id") {
            Some(Value::String(id)) if nonblank(id).is_some() => id.trim().to_owned(),
            Some(Value::Number(id)) => id.to_string(),
            // Position is the only stable identity an id-less comment has.
            _ => format!("n{}", index + 1),
        };
        let comment_key = keys::comment(source, &format!("{key}-{id}"));
        let author = person(&mut unit, comment.get("author"));
        let name = match &author {
            Some((_, author)) => format!("{author} on {key}"),
            None => format!("Comment on {key}"),
        };
        unit.add_node(
            NodeAssertion::new(NodeKind::Comment, comment_key.clone(), name)
                .attr_opt(attrs::CREATED_AT, timestamp(comment.get("created")))
                .attr_opt(attrs::UPDATED_AT, timestamp(comment.get("updated"))),
        );
        unit.add_edge(EdgeAssertion::new(
            &thread,
            EdgeKind::Contains,
            &comment_key,
        ));
        if let Some((author, _)) = author {
            unit.add_edge(EdgeAssertion::new(
                &comment_key,
                EdgeKind::AuthoredBy,
                author,
            ));
        }
        mention_edges(&mut unit, &comment_key, &key, &body, false);
        unit.add_chunk(&comment_key, ChunkKind::Comment, body);
        has_comments = true;
    }
    if has_comments {
        let thread_status = if resolution.is_some() {
            attrs::THREAD_RESOLVED
        } else {
            attrs::THREAD_OPEN
        };
        unit.add_node(
            NodeAssertion::new(
                NodeKind::Thread,
                thread.clone(),
                format!("Comments on {key}"),
            )
            .attr(attrs::THREAD_KIND, "page")
            .attr(attrs::THREAD_STATUS, thread_status)
            .attr_opt(attrs::TITLE, summary),
        );
        unit.add_edge(EdgeAssertion::new(&ticket, EdgeKind::Contains, &thread));
    }

    Ok(unit.finish())
}
