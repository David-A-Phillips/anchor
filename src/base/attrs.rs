//! Well-known attribute names.
//!
//! Node and edge attributes are free-form JSON maps, but producers and consumers must agree on the
//! names of the attributes that drive behaviour (thread status, approval decisions, ...). Use these
//! constants rather than string literals.

/// Repository-relative path of a file or symbol (`String`).
pub const PATH: &str = "path";
/// First line of a symbol or section, 1-based (`u64`).
pub const LINE_START: &str = "line_start";
/// Last line of a symbol or section, 1-based inclusive (`u64`).
pub const LINE_END: &str = "line_end";
/// Programming or markup language (`String`, e.g. `"rust"`).
pub const LANGUAGE: &str = "language";
/// [`SymbolKind`](crate::base::SymbolKind) textual name (`String`).
pub const SYMBOL_KIND: &str = "symbol_kind";
/// One-line signature of a symbol (`String`).
pub const SIGNATURE: &str = "signature";
/// Visibility of a symbol: `"public"`, `"crate"`, `"private"` (`String`).
pub const VISIBILITY: &str = "visibility";
/// Canonical URL of the entity in its system of origin (`String`).
pub const URL: &str = "url";
/// Human title of a page, ticket, thread or pull request (`String`).
pub const TITLE: &str = "title";
/// Short free-text description (`String`).
pub const DESCRIPTION: &str = "description";
/// Workflow status of a ticket, API or pull request, lower-cased (`String`).
pub const STATUS: &str = "status";
/// Resolution of a ticket (`String`).
pub const RESOLUTION: &str = "resolution";
/// Priority of a ticket (`String`).
pub const PRIORITY: &str = "priority";
/// Tracker issue type (`String`, e.g. `"bug"`, `"epic"`).
pub const ISSUE_TYPE: &str = "issue_type";
/// Labels/tags (`[String]`).
pub const LABELS: &str = "labels";
/// Creation time, milliseconds since the Unix epoch (`i64`).
pub const CREATED_AT: &str = "created_at";
/// Last modification time, milliseconds since the Unix epoch (`i64`).
pub const UPDATED_AT: &str = "updated_at";
/// E-mail address of a person or team (`String`).
pub const EMAIL: &str = "email";
/// Role or job title (`String`).
pub const ROLE: &str = "role";
/// Version of an API definition or page (`String`).
pub const VERSION: &str = "version";
/// HTTP method of an API endpoint, upper-cased (`String`).
pub const METHOD: &str = "method";
/// Route template of an API endpoint (`String`).
pub const ROUTE: &str = "route";

/// Status of a comment thread (`String`): one of [`THREAD_OPEN`], [`THREAD_RESOLVED`],
/// [`THREAD_DISMISSED`].
pub const THREAD_STATUS: &str = "thread_status";
/// Thread status: still under discussion.
pub const THREAD_OPEN: &str = "open";
/// Thread status: concluded and addressed.
pub const THREAD_RESOLVED: &str = "resolved";
/// Thread status: closed without being addressed (dismissed review, rejected suggestion).
pub const THREAD_DISMISSED: &str = "dismissed";
/// Kind of thread (`String`): `"page"`, `"inline"` or `"review"`.
pub const THREAD_KIND: &str = "thread_kind";
/// Display name of whoever dismissed or resolved a thread (`String`).
pub const CLOSED_BY: &str = "closed_by";
/// Free-text reason recorded when a thread was dismissed or resolved (`String`).
pub const CLOSE_REASON: &str = "close_reason";
/// The text an inline thread is anchored to (`String`).
pub const ANCHOR_TEXT: &str = "anchor_text";

/// Decision of an approval record (`String`): one of [`DECISION_APPROVED`], [`DECISION_REJECTED`],
/// [`DECISION_CHANGES_REQUESTED`], [`DECISION_PENDING`].
pub const DECISION: &str = "decision";
/// Approval decision: approved.
pub const DECISION_APPROVED: &str = "approved";
/// Approval decision: rejected.
pub const DECISION_REJECTED: &str = "rejected";
/// Approval decision: changes requested.
pub const DECISION_CHANGES_REQUESTED: &str = "changes_requested";
/// Approval decision: not decided yet.
pub const DECISION_PENDING: &str = "pending";
/// Whether an approval record, thread or ticket link is currently blocking (`bool`).
pub const BLOCKING: &str = "blocking";
/// Free-text justification attached to a decision, block or relationship (`String`).
pub const REASON: &str = "reason";

/// Marks a node that was created only because something referred to it (`bool`).
pub const STUB: &str = "stub";
/// How a looked-up edge endpoint was resolved (`String`): `"exact"`, `"alias"`, `"scoped"`.
pub const RESOLUTION_METHOD: &str = "resolution_method";
