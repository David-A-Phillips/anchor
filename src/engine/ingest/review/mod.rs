//! Code-review exports: pull requests with their reviews and review threads.
//!
//! Dismissed and resolved review discussions are a primary source for "why" questions, so nothing
//! is dropped here: a dismissed review stays in the graph as an [`NodeKind::Approval`] marked
//! dismissed, and its inline threads stay as [`NodeKind::Thread`]s with `thread_status = dismissed`.
//!
//! # Input format
//!
//! A directory (scanned recursively) of `*.json` files, **one pull request per file**. Each file
//! holds the GitHub REST representations of one pull request combined into a single object:
//!
//! ```json
//! {
//!   "pull_request": {
//!     "number": 482, "title": "...", "body": "...",
//!     "state": "open" | "closed", "merged": true, "merged_at": "2026-01-02T03:04:05Z",
//!     "draft": false, "user": {"login": "asmith"}, "html_url": "https://...",
//!     "created_at": "...", "updated_at": "...",
//!     "base": {"ref": "main", "repo": {"name": "payments-api", "full_name": "acme/payments-api"}},
//!     "head": {"ref": "feature/PAY-123"},
//!     "labels": [{"name": "bug"}],
//!     "requested_reviewers": [{"login": "bob"}], "assignees": [{"login": "carol"}]
//!   },
//!   "reviews": [
//!     {"id": 1, "user": {"login": "bob"}, "body": "...", "submitted_at": "...",
//!      "state": "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED" | "PENDING"}
//!   ],
//!   "review_comments": [
//!     {"id": 10, "in_reply_to_id": null, "pull_request_review_id": 1, "user": {"login": "bob"},
//!      "body": "...", "path": "src/lib.rs", "line": 12, "original_line": 12,
//!      "diff_hunk": "@@ ...", "created_at": "..."}
//!   ],
//!   "issue_comments": [{"id": 20, "user": {"login": "carol"}, "body": "...", "created_at": "..."}],
//!   "review_threads": [
//!     {"id": "T1", "is_resolved": true, "is_outdated": false, "resolved_by": {"login": "bob"},
//!      "comments": [10, 11]}
//!   ]
//! }
//! ```
//!
//! These are the payloads of `GET /repos/{o}/{r}/pulls/{n}`, `.../pulls/{n}/reviews`,
//! `.../pulls/{n}/comments` and `.../issues/{n}/comments`. `review_threads` has no REST
//! equivalent: an exporter fills it from the GraphQL `pullRequest.reviewThreads` connection, listing
//! for each thread the REST (`databaseId`) ids of its comments.
//!
//! Rules of acceptance:
//!
//! * A **bare pull request object** (the `pull_request` value itself at the top level) is accepted;
//!   the four arrays may then sit next to its fields or be absent.
//! * Every array is optional, and so is every field except `number` (a positive integer). The
//!   repository name is taken from `base.repo.name`, else the last segment of
//!   `base.repo.full_name`, else [`ReviewOptions::repository`], else the `html_url`
//!   (`.../<owner>/<repo>/pull/<n>`); a document for which none is available is rejected.
//! * Ids may be JSON numbers or strings. `user` may be `null` (deleted accounts); a string is
//!   accepted in place of a `{login}` object. Unknown fields are ignored; fields of an unexpected
//!   type are treated as absent. Comments and reviews without an `id` are skipped with a warning.
//! * Timestamps are RFC 3339. `state` values are matched case-insensitively.
//! * Optional extensions (not part of GitHub's REST payloads, honoured when an exporter provides
//!   them): on a `DISMISSED` review, `dismissed_state` (the state before dismissal, e.g.
//!   `"APPROVED"`), `dismissed_by {login}` and `dismissal_message`; in `review_threads`, the
//!   GraphQL spellings `isResolved`, `isOutdated`, `resolvedBy` are accepted as well.
//!
//! # Output
//!
//! One [`Unit`] of kind [`UnitKind::PullRequest`] per pull request, keyed
//! `<source>/pull_request/<repo slug>#<number>`:
//!
//! | Node | Key | Notes |
//! |------|-----|-------|
//! | PullRequest | `keys::pull_request(repo, n)` | aliases `#<n>`, `<repo>#<n>`; attrs `title`, `status` (`open`, `closed`, `merged`, `draft`), `url`, `labels`, `created_at`, `updated_at`; Description chunk |
//! | Person | via [`party`](super::party) | logins are handles |
//! | Thread (review) | `thread:<source>:<repo slug>-pr-<n>-thread-<root comment id>` | `thread_kind = review`, `thread_status`, `anchor_text` (`path:line`), `path`, `line_start`, [`ATTR_OUTDATED`], `closed_by`, `close_reason`; Code chunk with the tail of the diff hunk |
//! | Thread (conversation) | `thread:<source>:<repo slug>-pr-<n>-conversation` | `thread_kind = page`, `thread_status = open` |
//! | Comment | `comment:<source>:<repo slug>-pr-<n>-review-comment-<id>` / `...-comment-<id>` | `created_at`, `url`; Comment chunk |
//! | Approval | `keys::approval(source, "pr-<n>-review-<id>")` | `decision`, `blocking`, `reason`, [`ATTR_REVIEW_STATE`], [`ATTR_DISMISSED`], `created_at`; Decision chunk |
//!
//! Edges: `pr -AuthoredBy-> person`; `pr -AssignedTo-> person` (`role` = `assignee` or
//! `reviewer`); `pr -Tracks-> Lookup([Service], repo)`; `pr -Tracks-> Lookup([Ticket], key)` for
//! tracker keys in the title, body or head branch; `pr -Mentions-> ...` for other candidates in the
//! title and body; `pr -Contains-> thread -Contains-> comment`; `comment -ReplyTo-> comment`;
//! `comment -AuthoredBy-> person`; `comment -Mentions-> ...`; `thread -Discusses->
//! Lookup([File], path)`; `thread -RelatesTo-> approval` (the review a thread was opened by);
//! `approval -Approves-> pr`; `approval -DecidedBy-> person`; `approval -Blocks-> pr` (with
//! `reason`) for blocking reviews.
//!
//! Semantics worth knowing:
//!
//! * A review thread is the set of review comments sharing a root through `in_reply_to_id`. Its
//!   status is `resolved` when `review_threads` says so, else `dismissed` when the review owning
//!   the root comment is `DISMISSED`, else `open`. It is outdated when `review_threads` says so or,
//!   lacking that, when the root comment has `"line": null` with an `original_line`.
//! * A review becomes an Approval when it has a body or a decisive state (`APPROVED`,
//!   `CHANGES_REQUESTED`, `DISMISSED`). `COMMENTED` and `PENDING` map to `decision = pending`. A
//!   dismissed review keeps `dismissed_state` when exported, else `changes_requested`, and carries
//!   `dismissed = true` and `thread_status = dismissed`.
//! * A review is **blocking** when it is `CHANGES_REQUESTED`, the pull request is `open` or
//!   `draft`, and it is its author's most recent decisive review (a later approval by the same
//!   reviewer lifts the block, as on GitHub).
//! * Output is a pure function of the input: same document, same unit content hash.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::party::PersonIdentity;
use super::{IngestOutput, mentions};
use crate::base::time::parse_timestamp_ms;
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, Lookup, NodeAssertion, NodeKey, NodeKind, Result,
    SourceId, Unit, UnitKind, attrs, keys,
};

/// Thread attribute (`bool`): the code a review thread is anchored to has changed since.
pub const ATTR_OUTDATED: &str = "outdated";
/// Approval attribute (`bool`): the review was dismissed.
pub const ATTR_DISMISSED: &str = "dismissed";
/// Approval attribute (`String`): the review state as exported, lower-cased (`approved`,
/// `changes_requested`, `commented`, `dismissed`, `pending`).
pub const ATTR_REVIEW_STATE: &str = "review_state";
/// `thread_kind` of inline review threads.
pub const THREAD_KIND_REVIEW: &str = "review";
/// `thread_kind` of the pull request conversation.
pub const THREAD_KIND_PAGE: &str = "page";
/// `role` of an `AssignedTo` edge to an assignee.
pub const ROLE_ASSIGNEE: &str = "assignee";
/// `role` of an `AssignedTo` edge to a requested reviewer.
pub const ROLE_REVIEWER: &str = "reviewer";

/// Longest `reason` excerpt taken from a review body, in characters.
const REASON_CHARS: usize = 240;
/// Diff hunks are context for a thread, not content: only the lines nearest the anchor are kept.
const HUNK_LINES: usize = 12;

/// How a code-review source is ingested.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewOptions {
    /// Repository name used when a document does not name its repository.
    pub repository: Option<String>,
    /// Source id of the repository the pull requests belong to. When set, file and service
    /// lookups prefer nodes asserted by that source.
    pub repo_source: Option<SourceId>,
}

/// Ingests every `*.json` file under `dir` (recursively) as one pull request each.
///
/// A file that cannot be read or parsed is reported as a warning and skipped, as is a second file
/// describing a pull request already seen. Files are visited in path order, so the output is
/// deterministic. Fails only when `dir` itself is missing or not a directory.
///
/// Blocking (filesystem and parsing): from async code use [`scan_reviews_async`].
pub fn scan_reviews(
    dir: &Path,
    source: &SourceId,
    options: &ReviewOptions,
) -> Result<IngestOutput> {
    if !dir.is_dir() {
        return Err(Error::not_found(format!(
            "code-review export directory `{}`",
            dir.display()
        )));
    }
    let mut output = IngestOutput::default();
    let mut seen = HashSet::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let location = err
                    .path()
                    .map(|path| relative(dir, path))
                    .unwrap_or_default();
                output.warn(location, format!("cannot read: {err}"));
                continue;
            }
        };
        let path = entry.path();
        let is_json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if !entry.file_type().is_file() || !is_json {
            continue;
        }
        let location = relative(dir, path);
        let parsed = std::fs::read_to_string(path)
            .map_err(Error::from)
            .and_then(|text| parse_pull_request(&text, &location, source, options));
        match parsed {
            Ok(document) => {
                output.warnings.extend(document.warnings);
                for unit in document.units {
                    if seen.insert(unit.key.clone()) {
                        output.units.push(unit);
                    } else {
                        output.warn(
                            location.as_str(),
                            format!("duplicate pull request `{}` ignored", unit.key.as_str()),
                        );
                    }
                }
            }
            Err(err) => output.warn(location, err.to_string()),
        }
    }
    Ok(output)
}

/// [`scan_reviews`] on the blocking thread pool.
pub async fn scan_reviews_async(
    dir: PathBuf,
    source: SourceId,
    options: ReviewOptions,
) -> Result<IngestOutput> {
    tokio::task::spawn_blocking(move || scan_reviews(&dir, &source, &options)).await?
}

/// Ingests one exported pull request from its JSON text.
///
/// `location` names the document in warnings (a path relative to the source root). Returns one
/// canonical unit plus the warnings about items that were skipped inside the document; fails with
/// [`Error::Parse`] when the text is not JSON or does not describe a pull request.
pub fn parse_pull_request(
    json: &str,
    location: &str,
    source: &SourceId,
    options: &ReviewOptions,
) -> Result<IngestOutput> {
    let document: Value =
        serde_json::from_str(json).map_err(|err| Error::parse(format!("invalid JSON: {err}")))?;
    ingest_pull_request(&document, location, source, options)
}

/// Ingests one exported pull request from an already parsed document (see the module docs for the
/// accepted shape). Same contract as [`parse_pull_request`].
pub fn ingest_pull_request(
    document: &Value,
    location: &str,
    source: &SourceId,
    options: &ReviewOptions,
) -> Result<IngestOutput> {
    let pull = match document.get("pull_request") {
        Some(pull) if pull.is_object() => pull,
        _ if document.get("number").is_some() => document,
        _ => {
            return Err(Error::parse(
                "not a pull request export: expected an object with `pull_request` or `number`",
            ));
        }
    };
    let number = pull
        .get("number")
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| Error::parse("pull request has no positive integer `number`"))?;
    let repo = repository_name(pull, options)
        .ok_or_else(|| Error::parse(format!("pull request #{number} does not name its repository (no `base.repo.name`, `html_url` or configured repository)")))?;
    let slug = keys::slugify(&repo);
    if slug.is_empty() {
        return Err(Error::parse(format!(
            "pull request #{number} has an unusable repository name `{repo}`"
        )));
    }

    let pr_key = keys::pull_request(&repo, number);
    let mut builder = Builder {
        unit: Unit::new(
            keys::unit(source, UnitKind::PullRequest, &format!("{slug}#{number}")),
            source.clone(),
            UnitKind::PullRequest,
        ),
        output: IngestOutput::default(),
        source,
        options,
        location,
        pr: pr_key,
        label: format!("{repo}#{number}"),
        local: format!("{slug}-pr-{number}"),
        number,
    };
    let status = builder.pull_request(pull, &repo);
    let reviews = builder.reviews(items(document, "reviews"), &status);
    builder.review_threads(
        items(document, "review_comments"),
        items(document, "review_threads"),
        &reviews,
    );
    builder.conversation(items(document, "issue_comments"));

    let mut output = builder.output;
    output.units.push(builder.unit.finish());
    Ok(output)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A trimmed, non-empty string field.
fn text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// An identifier field: GitHub REST ids are numbers, GraphQL ids are strings.
fn id(value: &Value, field: &str) -> Option<String> {
    id_of(value.get(field)?)
}

fn id_of(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        _ => None,
    }
}

/// The login of a `{login}` object (or a plain string) under `field`.
fn login<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    let user = value.get(field)?;
    match user {
        Value::String(_) => text(value, field),
        _ => text(user, "login"),
    }
    .map(|login| login.trim_start_matches('@'))
    .filter(|login| !login.is_empty())
}

fn items<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn flag(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn timestamp(value: &Value, field: &str) -> Option<i64> {
    text(value, field).and_then(parse_timestamp_ms)
}

fn repository_name(pull: &Value, options: &ReviewOptions) -> Option<String> {
    let repo = pull.get("base").and_then(|base| base.get("repo"));
    let last = |path: &str| {
        path.rsplit('/')
            .next()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    repo.and_then(|repo| text(repo, "name"))
        .map(str::to_owned)
        .or_else(|| repo.and_then(|repo| text(repo, "full_name")).and_then(last))
        .or_else(|| {
            options
                .repository
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            // `https://host/<owner>/<repo>/pull/<n>`: the repository precedes `/pull/`.
            let (before, _) = text(pull, "html_url")?.rsplit_once("/pull/")?;
            last(before)
        })
}

/// Whitespace-collapsed prefix of `body`, short enough for an attribute.
fn excerpt(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= REASON_CHARS {
        return collapsed;
    }
    let mut short: String = collapsed.chars().take(REASON_CHARS).collect();
    short.push('…');
    short
}

fn decision_of(state: &str) -> Option<&'static str> {
    match state {
        "approved" => Some(attrs::DECISION_APPROVED),
        "changes_requested" => Some(attrs::DECISION_CHANGES_REQUESTED),
        "commented" | "pending" => Some(attrs::DECISION_PENDING),
        _ => None,
    }
}

/// What review threads need to know about a review.
struct ReviewInfo<'a> {
    dismissed: bool,
    /// The Approval node, when the review produced one.
    approval: Option<NodeKey>,
    value: &'a Value,
}

struct Builder<'a> {
    unit: Unit,
    output: IngestOutput,
    source: &'a SourceId,
    options: &'a ReviewOptions,
    location: &'a str,
    pr: NodeKey,
    /// `<repo>#<number>`, for display names.
    label: String,
    /// `<repo slug>-pr-<number>`, prefix of thread and comment ids.
    local: String,
    number: u64,
}

impl<'a> Builder<'a> {
    fn warn(&mut self, message: String) {
        self.output.warn(self.location, message);
    }

    /// Asserts the person behind a login and returns their key.
    fn person(&mut self, login: Option<&str>) -> Option<NodeKey> {
        let node = PersonIdentity::new(None, login, None).assertion()?;
        Some(self.unit.add_node(node))
    }

    fn scoped(&self, lookup: Lookup) -> Lookup {
        match &self.options.repo_source {
            Some(repo_source) => lookup.in_source(repo_source.clone()),
            None => lookup,
        }
    }

    /// Asserts the pull request node with its people, links and description; returns its status.
    fn pull_request(&mut self, pull: &Value, repo: &str) -> String {
        let merged = flag(pull, "merged") || text(pull, "merged_at").is_some();
        let closed = text(pull, "state").is_some_and(|state| state.eq_ignore_ascii_case("closed"));
        let status = if merged {
            "merged"
        } else if closed {
            "closed"
        } else if flag(pull, "draft") {
            "draft"
        } else {
            "open"
        };
        let labels: BTreeSet<&str> = items(pull, "labels")
            .iter()
            .filter_map(|label| {
                label
                    .as_str()
                    .map(str::trim)
                    .or_else(|| text(label, "name"))
            })
            .filter(|label| !label.is_empty())
            .collect();
        let title = text(pull, "title");
        let pr = self.pr.clone();
        let mut node = NodeAssertion::new(
            NodeKind::PullRequest,
            pr.clone(),
            title.unwrap_or(&self.label),
        )
        .alias(format!("#{}", self.number))
        .alias(self.label.clone())
        .attr(attrs::STATUS, status)
        .attr_opt(attrs::TITLE, title)
        .attr_opt(attrs::URL, text(pull, "html_url"))
        .attr_opt(attrs::CREATED_AT, timestamp(pull, "created_at"))
        .attr_opt(attrs::UPDATED_AT, timestamp(pull, "updated_at"));
        if !labels.is_empty() {
            node = node.attr(
                attrs::LABELS,
                labels.into_iter().map(Value::from).collect::<Vec<_>>(),
            );
        }
        self.unit.add_node(node);

        if let Some(author) = self.person(login(pull, "user")) {
            self.unit
                .add_edge(EdgeAssertion::new(&pr, EdgeKind::AuthoredBy, author));
        }
        // Assignees first: canonicalisation keeps the first of two equal edges, and someone who is
        // both is more usefully reported as the assignee.
        for (field, role) in [
            ("assignees", ROLE_ASSIGNEE),
            ("requested_reviewers", ROLE_REVIEWER),
        ] {
            for user in items(pull, field) {
                let handle = user.as_str().or_else(|| text(user, "login"));
                if let Some(person) = self.person(handle) {
                    self.unit.add_edge(
                        EdgeAssertion::new(&pr, EdgeKind::AssignedTo, person)
                            .attr(attrs::ROLE, role),
                    );
                }
            }
        }

        let service = self.scoped(Lookup::new(&[NodeKind::Service], repo));
        self.unit
            .add_edge(EdgeAssertion::new(&pr, EdgeKind::Tracks, service));

        let body = text(pull, "body").unwrap_or_default();
        let prose = format!("{}\n{body}", title.unwrap_or_default());
        for mention in mentions::extract_mentions(&prose) {
            let kind = if mention.lookup.kinds == [NodeKind::Ticket] {
                EdgeKind::Tracks
            } else {
                EdgeKind::Mentions
            };
            self.unit
                .add_edge(EdgeAssertion::new(&pr, kind, mention.lookup).weight(mention.weight));
        }
        // Branches are routinely named after the ticket they implement.
        let head = pull.get("head").and_then(|head| text(head, "ref"));
        for mention in mentions::extract_mentions(head.unwrap_or_default()) {
            if mention.lookup.kinds == [NodeKind::Ticket] {
                self.unit.add_edge(
                    EdgeAssertion::new(&pr, EdgeKind::Tracks, mention.lookup)
                        .weight(mention.weight),
                );
            }
        }
        self.unit.add_chunk(&pr, ChunkKind::Description, body);
        status.to_owned()
    }

    /// Asserts an Approval per review worth keeping; returns every review by id.
    fn reviews(&mut self, reviews: &'a [Value], status: &str) -> HashMap<String, ReviewInfo<'a>> {
        let mut ordered: Vec<(String, String, &Value)> = Vec::new();
        let mut seen = HashSet::new();
        for review in reviews {
            let Some(review_id) = id(review, "id") else {
                self.warn("review without `id` skipped".to_owned());
                continue;
            };
            if !seen.insert(review_id.clone()) {
                self.warn(format!("duplicate review `{review_id}` skipped"));
                continue;
            }
            let state = text(review, "state")
                .unwrap_or("commented")
                .to_ascii_lowercase();
            ordered.push((review_id, state, review));
        }
        // Chronological, so that "the reviewer's latest decisive review" is well defined whatever
        // the order of the export. Unsubmitted (pending) reviews sort last.
        ordered.sort_by_key(|(review_id, _, review)| {
            (
                timestamp(review, "submitted_at").unwrap_or(i64::MAX),
                review_id.parse::<u64>().unwrap_or(u64::MAX),
                review_id.clone(),
            )
        });
        let mut latest_decisive: HashMap<&str, &str> = HashMap::new();
        for (review_id, state, review) in &ordered {
            if matches!(
                state.as_str(),
                "approved" | "changes_requested" | "dismissed"
            ) {
                latest_decisive.insert(login(review, "user").unwrap_or_default(), review_id);
            }
        }

        let pr_is_open = matches!(status, "open" | "draft");
        let pr = self.pr.clone();
        let mut known = HashMap::new();
        for (review_id, state, review) in &ordered {
            let dismissed = state == "dismissed";
            let body = text(review, "body").unwrap_or_default();
            let decisive = matches!(
                state.as_str(),
                "approved" | "changes_requested" | "dismissed"
            );
            let mut approval = None;
            if decisive || !body.is_empty() {
                let reviewer = login(review, "user");
                let decision = if dismissed {
                    text(review, "dismissed_state")
                        .and_then(|before| decision_of(&before.to_ascii_lowercase()))
                        .unwrap_or(attrs::DECISION_CHANGES_REQUESTED)
                } else {
                    decision_of(state).unwrap_or(attrs::DECISION_PENDING)
                };
                let blocking = state == "changes_requested"
                    && pr_is_open
                    && latest_decisive.get(reviewer.unwrap_or_default())
                        == Some(&review_id.as_str());
                let reason = excerpt(body);
                let key = keys::approval(
                    self.source,
                    &format!("pr-{}-review-{review_id}", self.number),
                );
                let name = format!(
                    "Review of {} by {}: {}",
                    self.label,
                    reviewer.unwrap_or("unknown"),
                    if dismissed { "dismissed" } else { decision }
                );
                let mut node = NodeAssertion::new(NodeKind::Approval, key.clone(), name)
                    .attr(attrs::DECISION, decision)
                    .attr(attrs::BLOCKING, blocking)
                    .attr(ATTR_REVIEW_STATE, state.as_str())
                    .attr(ATTR_DISMISSED, dismissed)
                    .attr_opt(attrs::REASON, (!reason.is_empty()).then(|| reason.clone()))
                    .attr_opt(attrs::URL, text(review, "html_url"))
                    .attr_opt(attrs::CREATED_AT, timestamp(review, "submitted_at"));
                if dismissed {
                    node = node
                        .attr(attrs::THREAD_STATUS, attrs::THREAD_DISMISSED)
                        .attr_opt(attrs::CLOSED_BY, login(review, "dismissed_by"))
                        .attr_opt(attrs::CLOSE_REASON, text(review, "dismissal_message"));
                }
                self.unit.add_node(node);
                self.unit
                    .add_edge(EdgeAssertion::new(&key, EdgeKind::Approves, &pr));
                if let Some(person) = self.person(reviewer) {
                    self.unit
                        .add_edge(EdgeAssertion::new(&key, EdgeKind::DecidedBy, person));
                }
                if blocking {
                    let mut edge = EdgeAssertion::new(&key, EdgeKind::Blocks, &pr);
                    if !reason.is_empty() {
                        edge = edge.attr(attrs::REASON, reason);
                    }
                    self.unit.add_edge(edge);
                }
                self.unit.add_chunk(&key, ChunkKind::Decision, body);
                for edge in mentions::mention_edges(&key, body) {
                    self.unit.add_edge(edge);
                }
                approval = Some(key);
            }
            known.insert(
                review_id.clone(),
                ReviewInfo {
                    dismissed,
                    approval,
                    value: review,
                },
            );
        }
        known
    }

    /// Asserts one Comment node under `thread` and returns its key.
    fn comment(&mut self, thread: &NodeKey, local_id: &str, comment: &Value) -> NodeKey {
        let author = login(comment, "user");
        let key = keys::comment(self.source, local_id);
        self.unit.add_node(
            NodeAssertion::new(
                NodeKind::Comment,
                key.clone(),
                format!("{} on {}", author.unwrap_or("unknown"), self.label),
            )
            .attr_opt(attrs::CREATED_AT, timestamp(comment, "created_at"))
            .attr_opt(attrs::URL, text(comment, "html_url")),
        );
        self.unit
            .add_edge(EdgeAssertion::new(thread, EdgeKind::Contains, &key));
        if let Some(person) = self.person(author) {
            self.unit
                .add_edge(EdgeAssertion::new(&key, EdgeKind::AuthoredBy, person));
        }
        let body = text(comment, "body").unwrap_or_default();
        self.unit.add_chunk(&key, ChunkKind::Comment, body);
        for edge in mentions::mention_edges(&key, body) {
            self.unit.add_edge(edge);
        }
        key
    }

    /// Groups review comments into threads by their reply chains.
    fn review_threads(
        &mut self,
        comments: &[Value],
        thread_states: &[Value],
        reviews: &HashMap<String, ReviewInfo<'_>>,
    ) {
        let mut by_id: BTreeMap<String, &Value> = BTreeMap::new();
        for comment in comments {
            match id(comment, "id") {
                None => self.warn("review comment without `id` skipped".to_owned()),
                Some(comment_id) if by_id.contains_key(&comment_id) => {
                    self.warn(format!("duplicate review comment `{comment_id}` skipped"));
                }
                Some(comment_id) => {
                    by_id.insert(comment_id, comment);
                }
            }
        }
        // A reply whose parent was not exported starts its own thread rather than vanishing.
        let parent = |comment_id: &str| -> Option<String> {
            let parent = id(by_id.get(comment_id)?, "in_reply_to_id")?;
            (parent != comment_id && by_id.contains_key(&parent)).then_some(parent)
        };
        let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for comment_id in by_id.keys() {
            let mut chain = vec![comment_id.clone()];
            let mut root = comment_id.clone();
            while let Some(next) = parent(&root) {
                // Malformed exports can contain reply cycles. Every member of a cycle must agree
                // on the root, so the smallest id of the cycle is used.
                if let Some(start) = chain.iter().position(|seen| *seen == next) {
                    root = chain[start..].iter().min().cloned().unwrap_or(next);
                    break;
                }
                chain.push(next.clone());
                root = next;
            }
            groups.entry(root).or_default().push(comment_id);
        }
        let mut state_of: HashMap<String, &Value> = HashMap::new();
        for state in thread_states {
            for member in items(state, "comments") {
                // Exporters may list bare ids or `{id}` objects.
                if let Some(member) = id_of(member).or_else(|| id(member, "id")) {
                    state_of.entry(member).or_insert(state);
                }
            }
        }

        let pr = self.pr.clone();
        for (root_id, members) in &groups {
            let root = by_id[root_id];
            let state = std::iter::once(root_id.as_str())
                .chain(members.iter().copied())
                .find_map(|member| state_of.get(member).copied());
            let review = id(root, "pull_request_review_id").and_then(|review| reviews.get(&review));
            let resolved =
                state.is_some_and(|state| flag(state, "is_resolved") || flag(state, "isResolved"));
            let dismissed = review.is_some_and(|review| review.dismissed);
            let (status, closed_by, close_reason) = if resolved {
                let by = state.and_then(|state| {
                    login(state, "resolved_by").or_else(|| login(state, "resolvedBy"))
                });
                (attrs::THREAD_RESOLVED, by, None)
            } else if dismissed {
                let review = review.map(|review| review.value);
                (
                    attrs::THREAD_DISMISSED,
                    review.and_then(|review| login(review, "dismissed_by")),
                    review.and_then(|review| text(review, "dismissal_message")),
                )
            } else {
                (attrs::THREAD_OPEN, None, None)
            };
            let line = |field: &str| root.get(field).and_then(Value::as_u64);
            let outdated = match state {
                Some(state) => flag(state, "is_outdated") || flag(state, "isOutdated"),
                // REST nulls `line` once the anchored code is gone and keeps `original_line`.
                None => {
                    root.get("line").is_some_and(Value::is_null) && line("original_line").is_some()
                }
            };
            let path = text(root, "path").map(keys::normalize_path);
            let anchor_line = line("line").or_else(|| line("original_line"));
            let anchor = path.as_ref().map(|path| match anchor_line {
                Some(line) => format!("{path}:{line}"),
                None => path.clone(),
            });

            let thread = keys::thread(self.source, &format!("{}-thread-{root_id}", self.local));
            let name = match &anchor {
                Some(anchor) => format!("Review thread on {anchor} ({})", self.label),
                None => format!("Review thread on {}", self.label),
            };
            self.unit.add_node(
                NodeAssertion::new(NodeKind::Thread, thread.clone(), name)
                    .attr(attrs::THREAD_KIND, THREAD_KIND_REVIEW)
                    .attr(attrs::THREAD_STATUS, status)
                    .attr(ATTR_OUTDATED, outdated)
                    .attr_opt(attrs::ANCHOR_TEXT, anchor)
                    .attr_opt(attrs::PATH, path.clone())
                    .attr_opt(attrs::LINE_START, anchor_line)
                    .attr_opt(attrs::CLOSED_BY, closed_by)
                    .attr_opt(attrs::CLOSE_REASON, close_reason)
                    .attr_opt(attrs::CREATED_AT, timestamp(root, "created_at")),
            );
            self.unit
                .add_edge(EdgeAssertion::new(&pr, EdgeKind::Contains, &thread));
            if let Some(path) = path {
                let file = self.scoped(Lookup::new(&[NodeKind::File], path));
                self.unit
                    .add_edge(EdgeAssertion::new(&thread, EdgeKind::Discusses, file));
            }
            if let Some(approval) = review.and_then(|review| review.approval.as_ref()) {
                self.unit
                    .add_edge(EdgeAssertion::new(&thread, EdgeKind::RelatesTo, approval));
            }
            if let Some(hunk) = text(root, "diff_hunk") {
                let lines: Vec<&str> = hunk.lines().collect();
                let tail = lines[lines.len().saturating_sub(HUNK_LINES)..].join("\n");
                self.unit.add_chunk(&thread, ChunkKind::Code, tail);
            }

            let (source, prefix) = (self.source, format!("{}-review-comment-", self.local));
            for member in members {
                let local_id = format!("{prefix}{member}");
                let reply_to = parent(member)
                    .map(|parent| keys::comment(source, &format!("{prefix}{parent}")));
                let key = self.comment(&thread, &local_id, by_id[*member]);
                if let Some(reply_to) = reply_to {
                    self.unit
                        .add_edge(EdgeAssertion::new(&key, EdgeKind::ReplyTo, reply_to));
                }
            }
        }
    }

    /// Issue comments form one flat, page-level thread.
    fn conversation(&mut self, comments: &[Value]) {
        let thread = keys::thread(self.source, &format!("{}-conversation", self.local));
        let mut seen = HashSet::new();
        for comment in comments {
            let Some(comment_id) = id(comment, "id") else {
                self.warn("issue comment without `id` skipped".to_owned());
                continue;
            };
            if !seen.insert(comment_id.clone()) {
                self.warn(format!("duplicate issue comment `{comment_id}` skipped"));
                continue;
            }
            self.comment(
                &thread,
                &format!("{}-comment-{comment_id}", self.local),
                comment,
            );
        }
        if seen.is_empty() {
            return;
        }
        self.unit.add_node(
            NodeAssertion::new(
                NodeKind::Thread,
                thread.clone(),
                format!("Conversation on {}", self.label),
            )
            .attr(attrs::THREAD_KIND, THREAD_KIND_PAGE)
            .attr(attrs::THREAD_STATUS, attrs::THREAD_OPEN),
        );
        let pr = self.pr.clone();
        self.unit
            .add_edge(EdgeAssertion::new(&pr, EdgeKind::Contains, thread));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::base::NodeRef;

    fn source() -> SourceId {
        SourceId::new("reviews")
    }

    fn fixture() -> Value {
        json!({
            "pull_request": {
                "number": 482, "title": "PAY-123 Retry charges", "state": "open",
                "body": "Adds retries to `Ledger::charge`. Follow-up of OPS-9.",
                "merged": false, "draft": false, "user": {"login": "asmith"},
                "html_url": "https://github.com/acme/payments-api/pull/482",
                "created_at": "2026-01-02T03:04:05Z", "updated_at": "2026-01-03T00:00:00Z",
                "base": {"ref": "main", "repo": {"name": "payments-api", "full_name": "acme/payments-api"}},
                "head": {"ref": "feature/PAY-777-retries"},
                "labels": [{"name": "bug"}, {"name": "backend"}],
                "requested_reviewers": [{"login": "bob"}], "assignees": [{"login": "carol"}],
                "some_future_field": {"x": 1}
            },
            "reviews": [
                {"id": 1, "user": {"login": "bob"}, "state": "CHANGES_REQUESTED",
                 "body": "Retries must be idempotent.", "submitted_at": "2026-01-02T04:00:00Z"},
                {"id": 2, "user": {"login": "dave"}, "state": "DISMISSED",
                 "body": "Naming is wrong.", "submitted_at": "2026-01-02T05:00:00Z"},
                {"id": 3, "user": {"login": "erin"}, "state": "COMMENTED", "body": ""},
                {"id": 4, "user": {"login": "erin"}, "state": "APPROVED", "body": null,
                 "submitted_at": "2026-01-02T06:00:00Z"}
            ],
            "review_comments": [
                {"id": 10, "in_reply_to_id": null, "pull_request_review_id": 1,
                 "user": {"login": "bob"}, "body": "Why not use `RetryPolicy`? cc @carol",
                 "path": "src/ledger.rs", "line": 12, "original_line": 12,
                 "diff_hunk": "@@ -1,2 +1,3 @@\n fn charge() {\n+    retry();", "created_at": "2026-01-02T04:00:00Z"},
                {"id": 11, "in_reply_to_id": 10, "pull_request_review_id": 5,
                 "user": {"login": "asmith"}, "body": "Because it is not idempotent — ça casse 💥"},
                {"id": 12, "in_reply_to_id": 11, "user": {"login": "bob"}, "body": "ok"},
                {"id": 20, "pull_request_review_id": 2, "user": {"login": "dave"},
                 "body": "Rename this.", "path": "src/api.rs", "line": null, "original_line": 7},
                {"id": 30, "pull_request_review_id": 1, "user": null,
                 "body": "Old code.", "path": "src/old.rs", "line": 3},
                {"body": "no id"}
            ],
            "issue_comments": [
                {"id": 100, "user": {"login": "carol"}, "body": "Ship it after PAY-200."},
                {"id": 101, "user": {"login": "asmith"}, "body": "Will do."}
            ],
            "review_threads": [
                {"id": "T1", "is_resolved": true, "is_outdated": false,
                 "resolved_by": {"login": "bob"}, "comments": [10, 11, 12]},
                {"id": "T3", "is_resolved": false, "is_outdated": true, "comments": [30]}
            ]
        })
    }

    fn ingest(document: &Value) -> IngestOutput {
        ingest_pull_request(document, "pr.json", &source(), &ReviewOptions::default())
            .expect("ingests")
    }

    fn node<'a>(unit: &'a Unit, key: &str) -> &'a NodeAssertion {
        unit.nodes
            .iter()
            .find(|node| node.key.as_str() == key)
            .unwrap_or_else(|| panic!("node {key} missing"))
    }

    fn attr<'a>(node: &'a NodeAssertion, name: &str) -> &'a Value {
        node.attrs.get(name).unwrap_or(&Value::Null)
    }

    fn edge<'a>(unit: &'a Unit, from: &str, kind: EdgeKind, to: &str) -> Option<&'a EdgeAssertion> {
        unit.edges.iter().find(|edge| {
            let target = match &edge.to {
                NodeRef::Key(key) => key.as_str(),
                NodeRef::Lookup(lookup) => lookup.text.as_str(),
            };
            edge.kind == kind && edge.from == NodeRef::Key(NodeKey::new(from)) && target == to
        })
    }

    const PR: &str = "pr:payments-api#482";

    #[test]
    fn pull_request_node_people_and_links() {
        let output = ingest(&fixture());
        let unit = &output.units[0];
        assert_eq!(unit.key.as_str(), "reviews/pull_request/payments-api#482");
        assert_eq!(unit.kind, UnitKind::PullRequest);
        let pr = node(unit, PR);
        assert_eq!(pr.name, "PAY-123 Retry charges");
        assert_eq!(pr.aliases, vec!["#482", "payments-api#482"]);
        assert_eq!(attr(pr, attrs::STATUS), "open");
        assert_eq!(attr(pr, attrs::LABELS), &json!(["backend", "bug"]));
        assert_eq!(attr(pr, attrs::CREATED_AT), &json!(1_767_323_045_000_i64));
        assert!(attr(pr, attrs::URL).as_str().is_some());

        assert!(edge(unit, PR, EdgeKind::AuthoredBy, "person:asmith").is_some());
        let assignee = edge(unit, PR, EdgeKind::AssignedTo, "person:carol").expect("assignee");
        assert_eq!(assignee.attrs[attrs::ROLE], ROLE_ASSIGNEE);
        let reviewer = edge(unit, PR, EdgeKind::AssignedTo, "person:bob").expect("reviewer");
        assert_eq!(reviewer.attrs[attrs::ROLE], ROLE_REVIEWER);
        assert!(edge(unit, PR, EdgeKind::Tracks, "payments-api").is_some());
        // Ticket keys from the title, the body and the head branch.
        for ticket in ["PAY-123", "OPS-9", "PAY-777"] {
            assert!(
                edge(unit, PR, EdgeKind::Tracks, ticket).is_some(),
                "{ticket}"
            );
        }
        assert!(edge(unit, PR, EdgeKind::Mentions, "Ledger::charge").is_some());
        assert!(
            unit.chunks
                .iter()
                .any(|chunk| { chunk.node.as_str() == PR && chunk.kind == ChunkKind::Description })
        );
        // The comment without an id is reported, not fatal.
        assert_eq!(output.warnings.len(), 1);
        assert_eq!(output.warnings[0].location, "pr.json");
    }

    #[test]
    fn review_comments_are_grouped_into_threads_with_replies() {
        let output = ingest(&fixture());
        let unit = &output.units[0];
        let thread = "thread:reviews:payments-api-pr-482-thread-10";
        let comment = |id: u32| format!("comment:reviews:payments-api-pr-482-review-comment-{id}");
        let node_thread = node(unit, thread);
        assert_eq!(attr(node_thread, attrs::THREAD_KIND), THREAD_KIND_REVIEW);
        assert_eq!(attr(node_thread, attrs::ANCHOR_TEXT), "src/ledger.rs:12");
        assert!(edge(unit, PR, EdgeKind::Contains, thread).is_some());
        assert!(edge(unit, thread, EdgeKind::Discusses, "src/ledger.rs").is_some());
        for id in [10, 11, 12] {
            assert!(edge(unit, thread, EdgeKind::Contains, &comment(id)).is_some());
        }
        assert!(edge(unit, &comment(11), EdgeKind::ReplyTo, &comment(10)).is_some());
        assert!(edge(unit, &comment(12), EdgeKind::ReplyTo, &comment(11)).is_some());
        assert!(edge(unit, &comment(11), EdgeKind::AuthoredBy, "person:asmith").is_some());
        assert!(edge(unit, &comment(10), EdgeKind::Mentions, "RetryPolicy").is_some());
        assert!(edge(unit, &comment(10), EdgeKind::Mentions, "carol").is_some());
        // Unicode bodies survive verbatim.
        assert!(unit.chunks.iter().any(|chunk| {
            chunk.node.as_str() == comment(11)
                && chunk.kind == ChunkKind::Comment
                && chunk.text.ends_with("ça casse 💥")
        }));
        assert!(
            unit.chunks
                .iter()
                .any(|chunk| { chunk.node.as_str() == thread && chunk.kind == ChunkKind::Code })
        );
        // Three threads from review comments plus the conversation.
        let threads = unit.nodes.iter().filter(|n| n.kind == NodeKind::Thread);
        assert_eq!(threads.count(), 4);
    }

    #[test]
    fn thread_status_resolved_dismissed_and_outdated() {
        let output = ingest(&fixture());
        let unit = &output.units[0];
        let resolved = node(unit, "thread:reviews:payments-api-pr-482-thread-10");
        assert_eq!(attr(resolved, attrs::THREAD_STATUS), attrs::THREAD_RESOLVED);
        assert_eq!(attr(resolved, attrs::CLOSED_BY), "bob");
        assert_eq!(attr(resolved, ATTR_OUTDATED), false);

        // Owned by the dismissed review 2; outdated inferred from the nulled line.
        let dismissed = node(unit, "thread:reviews:payments-api-pr-482-thread-20");
        assert_eq!(
            attr(dismissed, attrs::THREAD_STATUS),
            attrs::THREAD_DISMISSED
        );
        assert_eq!(attr(dismissed, ATTR_OUTDATED), true);
        assert_eq!(attr(dismissed, attrs::ANCHOR_TEXT), "src/api.rs:7");
        assert!(
            edge(
                unit,
                "thread:reviews:payments-api-pr-482-thread-20",
                EdgeKind::RelatesTo,
                "approval:reviews:pr-482-review-2"
            )
            .is_some()
        );

        let outdated = node(unit, "thread:reviews:payments-api-pr-482-thread-30");
        assert_eq!(attr(outdated, attrs::THREAD_STATUS), attrs::THREAD_OPEN);
        assert_eq!(attr(outdated, ATTR_OUTDATED), true);
    }

    #[test]
    fn reviews_become_approvals_and_changes_requested_blocks() {
        let output = ingest(&fixture());
        let unit = &output.units[0];
        let blocking = "approval:reviews:pr-482-review-1";
        let review = node(unit, blocking);
        assert_eq!(
            attr(review, attrs::DECISION),
            attrs::DECISION_CHANGES_REQUESTED
        );
        assert_eq!(attr(review, attrs::BLOCKING), true);
        assert!(edge(unit, blocking, EdgeKind::Approves, PR).is_some());
        assert!(edge(unit, blocking, EdgeKind::DecidedBy, "person:bob").is_some());
        let blocks = edge(unit, blocking, EdgeKind::Blocks, PR).expect("blocks");
        assert_eq!(blocks.attrs[attrs::REASON], "Retries must be idempotent.");
        assert!(
            unit.chunks.iter().any(|chunk| {
                chunk.node.as_str() == blocking && chunk.kind == ChunkKind::Decision
            })
        );

        let dismissed = "approval:reviews:pr-482-review-2";
        let review = node(unit, dismissed);
        assert_eq!(
            attr(review, attrs::DECISION),
            attrs::DECISION_CHANGES_REQUESTED
        );
        assert_eq!(attr(review, ATTR_DISMISSED), true);
        assert_eq!(attr(review, attrs::THREAD_STATUS), attrs::THREAD_DISMISSED);
        assert_eq!(attr(review, attrs::BLOCKING), false);
        assert!(edge(unit, dismissed, EdgeKind::Blocks, PR).is_none());

        // An empty COMMENTED review is noise; a bodiless approval is a decision.
        assert!(
            !unit
                .nodes
                .iter()
                .any(|n| n.key.as_str().ends_with("review-3"))
        );
        let approved = node(unit, "approval:reviews:pr-482-review-4");
        assert_eq!(attr(approved, attrs::DECISION), attrs::DECISION_APPROVED);
    }

    #[test]
    fn blocking_is_lifted_by_merge_later_approval_and_known_dismissed_state() {
        let mut merged = fixture();
        merged["pull_request"]["state"] = json!("closed");
        merged["pull_request"]["merged_at"] = json!("2026-01-04T00:00:00Z");
        merged["reviews"][1]["dismissed_state"] = json!("APPROVED");
        merged["reviews"][1]["dismissed_by"] = json!({"login": "carol"});
        let unit = &ingest(&merged).units[0];
        assert_eq!(attr(node(unit, PR), attrs::STATUS), "merged");
        let review = node(unit, "approval:reviews:pr-482-review-1");
        assert_eq!(attr(review, attrs::BLOCKING), false);
        assert!(!unit.edges.iter().any(|edge| edge.kind == EdgeKind::Blocks));
        let dismissed = node(unit, "approval:reviews:pr-482-review-2");
        assert_eq!(attr(dismissed, attrs::DECISION), attrs::DECISION_APPROVED);
        assert_eq!(attr(dismissed, attrs::CLOSED_BY), "carol");
        let thread = node(unit, "thread:reviews:payments-api-pr-482-thread-20");
        assert_eq!(attr(thread, attrs::CLOSED_BY), "carol");

        let mut closed = fixture();
        closed["pull_request"]["state"] = json!("closed");
        assert_eq!(
            attr(node(&ingest(&closed).units[0], PR), attrs::STATUS),
            "closed"
        );

        let mut draft = fixture();
        draft["pull_request"]["draft"] = json!(true);
        // The same reviewer approving later supersedes their request for changes.
        draft["reviews"][3]["user"]["login"] = json!("bob");
        let unit = &ingest(&draft).units[0];
        assert_eq!(attr(node(unit, PR), attrs::STATUS), "draft");
        let review = node(unit, "approval:reviews:pr-482-review-1");
        assert_eq!(attr(review, attrs::BLOCKING), false);
    }

    #[test]
    fn issue_comments_form_one_conversation_thread() {
        let unit = &ingest(&fixture()).units[0];
        let thread = "thread:reviews:payments-api-pr-482-conversation";
        assert_eq!(
            attr(node(unit, thread), attrs::THREAD_KIND),
            THREAD_KIND_PAGE
        );
        assert!(edge(unit, PR, EdgeKind::Contains, thread).is_some());
        let first = "comment:reviews:payments-api-pr-482-comment-100";
        assert!(edge(unit, thread, EdgeKind::Contains, first).is_some());
        assert!(edge(unit, first, EdgeKind::AuthoredBy, "person:carol").is_some());
        assert!(edge(unit, first, EdgeKind::Mentions, "PAY-200").is_some());
    }

    #[test]
    fn bare_and_minimal_documents_are_accepted() {
        let options = ReviewOptions {
            repository: Some("Payments API".to_owned()),
            repo_source: Some(SourceId::new("payments-repo")),
        };
        let bare = json!({"number": 7, "title": "Tidy", "user": "zoë",
            "review_comments": [{"id": "a", "in_reply_to_id": "b", "path": "./src/lib.rs"},
                                {"id": "b", "in_reply_to_id": "a"}]});
        let output = ingest_pull_request(&bare, "7.json", &source(), &options).expect("bare");
        let unit = &output.units[0];
        assert!(output.warnings.is_empty());
        assert_eq!(unit.key.as_str(), "reviews/pull_request/payments-api#7");
        assert_eq!(attr(node(unit, "pr:payments-api#7"), attrs::STATUS), "open");
        assert!(
            edge(
                unit,
                "pr:payments-api#7",
                EdgeKind::AuthoredBy,
                "person:zoë"
            )
            .is_some()
        );
        // A reply cycle still yields exactly one thread holding both comments.
        let threads: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Thread)
            .collect();
        assert_eq!(threads.len(), 1);
        // Lookups are scoped to the repository source when it is known.
        let scoped = unit.edges.iter().filter_map(|edge| match &edge.to {
            NodeRef::Lookup(lookup) => lookup.scope.as_ref()?.source.as_ref(),
            NodeRef::Key(_) => None,
        });
        assert_eq!(scoped.count(), 2);

        // No repository anywhere but the URL; no arrays; odd types everywhere.
        let odd = json!({"pull_request": {"number": 9, "labels": "nope", "user": 5, "body": 12,
            "assignees": [null, 3], "html_url": "https://github.com/acme/ledger/pull/9"},
            "reviews": {"not": "an array"}});
        let unit = &ingest(&odd).units[0];
        assert_eq!(node(unit, "pr:ledger#9").name, "ledger#9");
        assert_eq!(unit.nodes.len(), 1);

        for rejected in [
            json!({"number": 0}),
            json!({"number": 3}),
            json!([1]),
            json!({"title": "x"}),
        ] {
            let result = ingest_pull_request(&rejected, "x", &source(), &ReviewOptions::default());
            assert!(matches!(result, Err(Error::Parse(_))), "{rejected}");
        }
    }

    #[test]
    fn output_is_deterministic_and_order_independent() {
        let first = ingest(&fixture());
        let again = ingest(&fixture());
        let mut shuffled = fixture();
        for field in [
            "reviews",
            "review_comments",
            "issue_comments",
            "review_threads",
        ] {
            shuffled[field].as_array_mut().expect("array").reverse();
        }
        let hash = |output: &IngestOutput| output.units[0].content_hash().expect("hash");
        assert_eq!(hash(&first), hash(&again));
        assert_eq!(hash(&first), hash(&ingest(&shuffled)));
    }

    #[tokio::test]
    async fn scan_reports_malformed_files_and_duplicates_as_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        };
        write("a/482.json", &fixture().to_string());
        write("b/again.JSON", &fixture().to_string());
        write("broken.json", "{ \"pull_request\": ");
        write("notes.txt", "ignored");
        let output = scan_reviews_async(dir.path().to_owned(), source(), ReviewOptions::default())
            .await
            .expect("scan");
        assert_eq!(output.units.len(), 1);
        let locations: Vec<_> = output
            .warnings
            .iter()
            .map(|w| w.location.as_str())
            .collect();
        // One skipped comment per copy of the fixture, the duplicate, and the broken file.
        assert_eq!(
            locations,
            ["a/482.json", "b/again.JSON", "b/again.JSON", "broken.json"]
        );
        assert!(output.warnings[3].message.contains("invalid JSON"));

        let missing = scan_reviews(
            &dir.path().join("nope"),
            &source(),
            &ReviewOptions::default(),
        );
        assert!(matches!(missing, Err(Error::NotFound(_))));
    }
}
