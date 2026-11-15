//! Comment threads as written in the sources (sidecar JSON, embedded HTML markup), before ids are
//! assigned and nodes are built. The formats are specified in the [module docs](super).

use scraper::ElementRef;
use serde_json::Value;

use crate::engine::ingest::docs::text;
use crate::engine::ingest::party::PersonIdentity;

/// Nesting depth of comment markup beyond which replies are no longer followed.
const MAX_COMMENT_DEPTH: usize = 100;

/// A thread as found in the source.
#[derive(Debug)]
pub(super) struct RawThread {
    pub id: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub anchor: Option<String>,
    pub closed_by: PersonIdentity,
    pub close_reason: Option<String>,
    pub closed_at: Option<String>,
    pub comments: Vec<RawComment>,
}

/// A comment as found in the source.
#[derive(Debug)]
pub(super) struct RawComment {
    pub id: Option<String>,
    pub author: PersonIdentity,
    pub created: Option<String>,
    pub body: String,
    /// Source id of the comment this one answers.
    pub reply_to: Option<String>,
    /// Index (within the thread) of the comment this one is nested in.
    pub parent: Option<usize>,
}

fn nobody() -> PersonIdentity {
    PersonIdentity::new(None, None, None)
}

/// A scalar as trimmed text; blank strings, arrays, objects and null are absent.
pub(super) fn scalar(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The first of `names` present in the object `value` as a scalar.
pub(super) fn field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(name).and_then(scalar))
}

/// A list field: an array of scalars, or one comma-separated string.
pub(super) fn list_field(value: &Value, names: &[&str]) -> Vec<String> {
    for name in names {
        match value.get(name) {
            Some(Value::Array(items)) => return items.iter().filter_map(scalar).collect(),
            Some(other) => {
                if let Some(text) = scalar(other) {
                    return split_list(&text);
                }
            }
            None => {}
        }
    }
    Vec::new()
}

/// Splits a comma-separated list, dropping blanks.
pub(super) fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A person written either as a string (`"Alice Smith <alice@example.com>"`, `"@asmith"`) or as
/// an object with `name`, `handle` and `email`.
pub(super) fn person(value: Option<&Value>) -> PersonIdentity {
    match value {
        Some(value @ Value::Object(_)) => {
            let name = field(value, &["name", "display_name", "displayName"]);
            let handle = field(value, &["handle", "username", "login"]);
            let email = field(value, &["email"]);
            PersonIdentity::new(
                name.as_deref(),
                handle
                    .as_deref()
                    .map(|handle| handle.trim_start_matches('@')),
                email.as_deref(),
            )
        }
        Some(other) => scalar(other).map_or_else(nobody, |text| PersonIdentity::parse(&text)),
        None => nobody(),
    }
}

/// Parses a `<stem>.comments.json` sidecar. Entries that are not objects are reported in
/// `problems` and skipped; a document that is not JSON, or has no thread list, is an error.
pub(super) fn parse_sidecar(
    bytes: &[u8],
    problems: &mut Vec<String>,
) -> Result<Vec<RawThread>, String> {
    let document: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("malformed comments JSON: {error}"))?;
    let entries = match &document {
        Value::Array(entries) => entries,
        Value::Object(map) => match map.get("threads") {
            Some(Value::Array(entries)) => entries,
            _ => return Err("comments JSON has no \"threads\" array".to_owned()),
        },
        _ => return Err("comments JSON must be an object with a \"threads\" array".to_owned()),
    };
    let mut threads = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        if !entry.is_object() {
            problems.push(format!("thread #{index} is not an object; skipped"));
            continue;
        }
        let mut comments = Vec::new();
        match entry.get("comments") {
            Some(Value::Array(items)) => {
                for (position, item) in items.iter().enumerate() {
                    if !item.is_object() {
                        problems.push(format!(
                            "comment #{position} of thread #{index} is not an object; skipped"
                        ));
                        continue;
                    }
                    comments.push(RawComment {
                        id: field(item, &["id"]),
                        author: person(item.get("author")),
                        created: field(item, &["created", "created_at"]),
                        body: field(item, &["body", "text"]).unwrap_or_default(),
                        reply_to: field(item, &["reply_to", "replyTo", "in_reply_to"]),
                        parent: None,
                    });
                }
            }
            Some(Value::Null) | None => {}
            Some(_) => problems.push(format!("\"comments\" of thread #{index} is not an array")),
        }
        threads.push(RawThread {
            id: field(entry, &["id"]),
            kind: field(entry, &["kind", "type"]),
            status: field(entry, &["status", "state"]),
            anchor: field(entry, &["anchor", "anchor_text"]),
            closed_by: person(entry.get("closed_by")),
            close_reason: field(entry, &["close_reason", "reason"]),
            closed_at: field(entry, &["closed_at"]),
            comments,
        });
    }
    Ok(threads)
}

fn has_class(element: &ElementRef<'_>, class: &str) -> bool {
    element
        .value()
        .classes()
        .any(|candidate| candidate == class)
}

/// `true` for an embedded thread container (`class="comment-thread"`).
pub(super) fn is_thread_container(element: &ElementRef<'_>) -> bool {
    has_class(element, "comment-thread")
}

fn is_comment(element: &ElementRef<'_>) -> bool {
    has_class(element, "comment")
}

fn attr(element: &ElementRef<'_>, name: &str) -> Option<String> {
    element
        .value()
        .attr(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// First descendant satisfying `wanted` that belongs to `element` itself rather than to a nested
/// comment.
fn find_own<'a>(
    element: ElementRef<'a>,
    wanted: &dyn Fn(&ElementRef<'a>) -> bool,
    depth: usize,
) -> Option<ElementRef<'a>> {
    if depth > MAX_COMMENT_DEPTH {
        return None;
    }
    for child in element.child_elements() {
        if is_comment(&child) {
            continue;
        }
        if wanted(&child) {
            return Some(child);
        }
        if let Some(found) = find_own(child, wanted, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn own_text(element: ElementRef<'_>, class: &str) -> Option<String> {
    let found = find_own(element, &|candidate| has_class(candidate, class), 0)?;
    let text = text::element_text(found, &|_| false).text;
    (!text.is_empty()).then_some(text)
}

fn is_comment_metadata(element: &ElementRef<'_>) -> bool {
    element.value().name() == "time"
        || [
            "comment-author",
            "comment-date",
            "comment-meta",
            "close-reason",
        ]
        .iter()
        .any(|class| has_class(element, class))
}

fn html_comment(element: ElementRef<'_>, parent: Option<usize>) -> RawComment {
    let author = attr(&element, "data-author")
        .or_else(|| own_text(element, "comment-author"))
        .map_or_else(nobody, |text| PersonIdentity::parse(&text));
    let created = attr(&element, "data-created").or_else(|| {
        find_own(element, &|candidate| candidate.value().name() == "time", 0)
            .and_then(|time| attr(&time, "datetime"))
    });
    // An explicit body element wins; otherwise the body is everything in the comment that is
    // neither metadata nor a nested reply.
    let body = own_text(element, "comment-body").unwrap_or_else(|| {
        text::element_text(element, &|candidate| {
            (is_comment(candidate) && *candidate != element) || is_comment_metadata(candidate)
        })
        .text
    });
    RawComment {
        id: attr(&element, "data-comment-id"),
        author,
        created,
        body,
        reply_to: attr(&element, "data-reply-to"),
        parent,
    }
}

fn collect_comments(
    element: ElementRef<'_>,
    parent: Option<usize>,
    depth: usize,
    out: &mut Vec<RawComment>,
) {
    if depth > MAX_COMMENT_DEPTH {
        return;
    }
    for child in element.child_elements() {
        if is_comment(&child) {
            let index = out.len();
            out.push(html_comment(child, parent));
            collect_comments(child, Some(index), depth + 1, out);
        } else {
            collect_comments(child, parent, depth + 1, out);
        }
    }
}

/// Reads one embedded thread container.
pub(super) fn html_thread(element: ElementRef<'_>) -> RawThread {
    let mut comments = Vec::new();
    collect_comments(element, None, 0, &mut comments);
    RawThread {
        id: attr(&element, "data-thread-id"),
        kind: attr(&element, "data-kind"),
        status: attr(&element, "data-status"),
        anchor: attr(&element, "data-anchor"),
        closed_by: attr(&element, "data-closed-by")
            .map_or_else(nobody, |text| PersonIdentity::parse(&text)),
        close_reason: attr(&element, "data-close-reason")
            .or_else(|| own_text(element, "close-reason")),
        closed_at: attr(&element, "data-closed-at"),
        comments,
    }
}
