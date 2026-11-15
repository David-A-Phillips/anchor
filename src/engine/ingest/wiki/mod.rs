//! Wiki snapshot ingestion: Markdown and HTML pages with their comment threads (page-level,
//! inline and review threads; open, resolved and dismissed).
//!
//! # Snapshot layout
//!
//! A snapshot is a directory tree. Every `*.md` / `*.markdown` and `*.html` / `*.htm` file is one
//! page; `<page stem>.comments.json` next to a page (`runbook.md` → `runbook.comments.json`) holds
//! its comment threads. Hidden files and directories and all other files are ignored. Files must
//! be UTF-8.
//!
//! ## Markdown pages
//!
//! Optional YAML front matter between `---` lines at the very top, all keys optional:
//!
//! ```yaml
//! ---
//! id: payments-runbook          # stable page id; default: slug of the path without extension
//! title: Payments API Runbook   # default: first `# heading`, else the file name
//! parent: Operations            # title (or id) of the parent page -> resolved by lookup
//! parent_id: operations         # id of the parent page -> linked by key (wins over `parent`)
//! labels: [payments, oncall]    # list or comma-separated string (alias: tags)
//! author: Alice Smith <alice@example.com>   # or "@asmith", or {name, handle, email}
//! created: 2024-03-01T09:00:00Z # RFC 3339, `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` (UTC)
//! updated: 2024-05-17T16:30:00Z
//! url: https://wiki.example.com/x/123
//! space: ENG
//! version: 7
//! documents: [payments-api, "POST /v1/charges"]   # entities this page is the documentation of
//! ---
//! ```
//!
//! The body is Markdown. `[text](relative/page.md)` and `[[Page Title]]` (also `[[Title|label]]`)
//! link to other pages.
//!
//! ## HTML pages
//!
//! * Title: `<meta name="title">`, else `<title>`, else the first `<h1>`, else the file name.
//! * Body: the first `<main>`, else the element with `id="main-content"`, else `<body>`. Scripts,
//!   styles, `<nav>`, forms, footers and breadcrumbs are dropped.
//! * Metadata from `<meta name="NAME" content="...">` (`property=` is accepted too; names are
//!   case-insensitive; the first name present wins):
//!
//! | Field | Accepted names |
//! |-------|----------------|
//! | id | `page-id`, `id`, `ajs-page-id`, `confluence-page-id` |
//! | title | `title`, `ajs-page-title`, `og:title`, `dc.title` |
//! | parent id | `parent-id`, `ajs-parent-page-id` |
//! | parent title | `parent`, `parent-title`, `ajs-parent-page-title` |
//! | labels (comma-separated) | `labels`, `keywords`, `tags`, `ajs-labels` |
//! | author | `author`, `creator`, `ajs-creator-name`, `dc.creator` |
//! | created | `created`, `date`, `dcterms.created`, `dc.date.created`, `article:published_time` |
//! | updated | `updated`, `modified`, `last-modified`, `dcterms.modified`, `dc.date.modified`, `article:modified_time` |
//! | url | `url`, `canonical`, `og:url`, or `<link rel="canonical" href>` |
//! | space | `space`, `space-key`, `ajs-space-key` |
//! | version | `version`, `ajs-page-version` |
//! | documents (comma-separated) | `documents` |
//!
//! ## Comment threads: sidecar JSON
//!
//! ```json
//! {"threads": [{
//!   "id": "t-204",
//!   "kind": "page | inline | review",
//!   "status": "open | resolved | dismissed",
//!   "anchor": "the quoted text an inline thread is attached to",
//!   "closed_by": "Bob Lee",
//!   "close_reason": "Superseded by the idempotency keys of PAY-123",
//!   "closed_at": "2024-05-02T10:00:00Z",
//!   "comments": [
//!     {"id": "c-1", "author": "Alice Smith", "created": "2024-05-01T09:00:00Z", "body": "..."},
//!     {"id": "c-2", "author": {"name": "Bob Lee", "handle": "blee", "email": "bob@example.com"},
//!      "created": "2024-05-01T10:00:00Z", "body": "...", "reply_to": "c-1"}
//!   ]}]}
//! ```
//!
//! A bare array of threads is accepted as well. Every field is optional. `kind` defaults to
//! `inline` when there is an anchor and `page` otherwise. Status synonyms: `closed`, `done`,
//! `fixed`, `accepted` → `resolved`; `rejected`, `declined`, `wontfix`, `outdated`, `obsolete` →
//! `dismissed`; anything else unknown → `open` with a warning. `reply_to` names a comment id of
//! the same thread. Ids may be strings or numbers.
//!
//! ## Comment threads: embedded HTML
//!
//! Anywhere in an HTML page (inside or outside the body element):
//!
//! ```html
//! <div class="comment-thread" data-thread-id="t-9" data-kind="review" data-status="dismissed"
//!      data-anchor="quoted text" data-closed-by="Bob Lee" data-close-reason="Out of scope"
//!      data-closed-at="2024-05-02T10:00:00Z">
//!   <div class="comment" data-comment-id="c-1" data-author="Alice Smith"
//!        data-created="2024-05-01T09:00:00Z">
//!     <div class="comment-body"><p>Should we retry?</p></div>
//!     <div class="comment" data-comment-id="c-2" data-author="Bob Lee">No.</div>
//!   </div>
//! </div>
//! ```
//!
//! Any element carrying the class works (not only `div`). A comment nested in another comment
//! replies to it unless `data-reply-to` says otherwise. Without a `comment-body` child the body is
//! the comment's own text; `comment-author` / `<time datetime>` children are fallbacks for the
//! `data-author` / `data-created` attributes, a `close-reason` child for `data-close-reason`. All
//! attributes are optional. Thread containers are not part of the page text. A page may have both
//! a sidecar and embedded threads; a thread id seen twice is ingested once (the sidecar wins).
//!
//! # Output
//!
//! One canonical [`Unit`] of kind [`UnitKind::WikiPage`] per page, keyed
//! `keys::unit(source, WikiPage, <page id>)`:
//!
//! * **Page** node `keys::wiki_page(source, <page id>)`; aliases: title, slug of the title, page
//!   id, path without extension; attrs `title`, `path`, `url`, `labels`, `created_at`,
//!   `updated_at`, `version`, `space`.
//! * `parent --Contains--> page`: by key for `parent_id`, else `Lookup([WikiPage], parent)`.
//! * `page --AuthoredBy--> person` (people are asserted through
//!   [`party`](crate::engine::ingest::party)).
//! * **Thread** nodes `keys::thread(source, "<page id>/<thread id>")` with attrs `thread_status`,
//!   `thread_kind`, `anchor_text`, `closed_by`, `close_reason`, `created_at`, `updated_at`,
//!   `closed_at`; `page --Contains--> thread`; `thread --DecidedBy--> person` for whoever closed
//!   it; `thread --Discusses-->` every mention candidate of its anchor text and first comment.
//! * **Comment** nodes `keys::comment(source, "<page id>/<thread id>/<comment id>")` with
//!   `created_at`; `thread --Contains--> comment`, `comment --ReplyTo--> comment`,
//!   `comment --AuthoredBy--> person`, `comment --Mentions-->` candidates of its body.
//! * Chunks: Text chunks on the page (sections split at headings, heading path on the first line,
//!   at most about [`WikiOptions::max_chunk_chars`]); one Comment chunk per comment; one Decision
//!   chunk per thread with a close reason (`"Dismissed by Bob Lee: <reason>"`).
//! * `page --Mentions-->` candidates in the title and text, and other pages it links to
//!   (`Lookup([WikiPage], <resolved path without extension>)` for relative links,
//!   `Lookup([WikiPage], <title>)` for `[[Title]]`, weight 0.9).
//! * `page --Documents-->` each `documents` entry and the page title (also with a trailing
//!   "Runbook", "Guide", ... removed), as lookups with weight 0.8.
//!
//! Thread and comment ids come from the source; missing ids are a stable hash of (page id,
//! ordinal, anchor, author, created). Thread and comment keys are scoped by page id because
//! exports commonly number them per page. Nothing depends on the clock or on directory iteration
//! order: the same input always yields the same unit content hash.
//!
//! Everything here is blocking, CPU-bound work: call it from a blocking thread.

mod threads;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use scraper::{ElementRef, Html};
use serde_json::Value;

use crate::base::time::parse_timestamp_ms;
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, Lookup, NodeAssertion, NodeKey, NodeKind, Result,
    SourceId, Unit, UnitKind, attrs, keys,
};
use crate::engine::ingest::docs::text::{self, DocLink, LinkKind};
use crate::engine::ingest::party::PersonIdentity;
use crate::engine::ingest::{IngestOutput, IngestWarning, mentions};

use threads::{RawThread, field, list_field, person};

/// Attribute holding the wiki space key of a page (`String`).
pub const ATTR_SPACE: &str = "space";
/// Attribute holding when a thread was closed, milliseconds since the Unix epoch (`i64`).
pub const ATTR_CLOSED_AT: &str = "closed_at";

/// Kinds a `documents` entry may name, in order of preference.
const DOCUMENTED_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::Domain,
    NodeKind::ApiEndpoint,
    NodeKind::Symbol,
    NodeKind::File,
];
/// Kinds a page title may name.
const TITLE_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::Domain,
];
/// Words that follow the name of the documented entity in page titles ("Payments API Runbook").
const TITLE_SUFFIXES: &[&str] = &[
    "runbook",
    "guide",
    "overview",
    "architecture",
    "design",
    "doc",
    "docs",
    "documentation",
    "readme",
    "handbook",
    "faq",
    "notes",
    "reference",
    "playbook",
    "spec",
    "specification",
    "manual",
];
const DOCUMENTS_WEIGHT: f32 = 0.8;
const PAGE_LINK_WEIGHT: f32 = 0.9;

/// Tuning knobs of wiki ingestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiOptions {
    /// Upper bound of a page Text chunk, in bytes of UTF-8.
    pub max_chunk_chars: usize,
    /// Pages and sidecars larger than this are skipped with a warning by [`scan_wiki`].
    pub max_file_bytes: u64,
}

impl Default for WikiOptions {
    fn default() -> Self {
        Self {
            max_chunk_chars: text::DEFAULT_CHUNK_CHARS,
            max_file_bytes: 16 * 1024 * 1024,
        }
    }
}

/// `true` when `path` has the extension of a wiki page (`md`, `markdown`, `html`, `htm`).
pub fn is_wiki_page(path: &Path) -> bool {
    page_format(&path.to_string_lossy()).is_some()
}

/// Where the comment sidecar of the page at `page` lives: `<stem>.comments.json` next to it.
pub fn sidecar_path(page: &Path) -> PathBuf {
    page.with_extension("comments.json")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageFormat {
    Markdown,
    Html,
}

fn page_format(path: &str) -> Option<PageFormat> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    match extension.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(PageFormat::Markdown),
        "html" | "htm" => Some(PageFormat::Html),
        _ => None,
    }
}

/// Ingests the wiki snapshot under `dir` as source `source`.
///
/// Pages are visited in lexicographic path order. One bad page never aborts the snapshot: an
/// unreadable, oversized, non-UTF-8 or duplicate page is skipped and reported as an
/// [`IngestWarning`] located at its snapshot-relative path, as are malformed sidecars (the page is
/// still ingested, without those threads) and sidecars without a page.
///
/// # Errors
/// Only when `dir` itself is not a readable directory.
pub fn scan_wiki(dir: &Path, source: &SourceId, options: &WikiOptions) -> Result<IngestOutput> {
    if !dir.is_dir() {
        return Err(Error::not_found(format!(
            "wiki snapshot directory {} does not exist",
            dir.display()
        )));
    }
    let mut output = IngestOutput::default();
    let mut pages: Vec<(String, PathBuf)> = Vec::new();
    let mut sidecars: BTreeSet<String> = BTreeSet::new();
    let walker = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let location = error
                    .path()
                    .map(|path| relative(dir, path))
                    .unwrap_or_default();
                output.warn(location, format!("cannot read: {error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = relative(dir, entry.path());
        if rel.to_ascii_lowercase().ends_with(".comments.json") {
            sidecars.insert(rel);
        } else if page_format(&rel).is_some() {
            pages.push((rel, entry.into_path()));
        }
    }

    let mut seen_units = HashSet::new();
    for (rel, path) in pages {
        let sidecar_rel = relative(dir, &sidecar_path(&path));
        let has_sidecar = sidecars.remove(&sidecar_rel);
        let bytes = match read_limited(&path, options.max_file_bytes) {
            Ok(bytes) => bytes,
            Err(message) => {
                output.warn(rel, message);
                continue;
            }
        };
        let sidecar = if has_sidecar {
            match read_limited(&sidecar_path(&path), options.max_file_bytes) {
                Ok(bytes) => Some(bytes),
                Err(message) => {
                    output.warn(sidecar_rel, message);
                    None
                }
            }
        } else {
            None
        };
        match parse_wiki_page(source, &rel, &bytes, sidecar.as_deref(), options) {
            Ok(page) => {
                let duplicate = page
                    .units
                    .iter()
                    .any(|unit| !seen_units.insert(unit.key.clone()));
                if duplicate {
                    // The skipped page's own problems are still worth reporting.
                    output.warnings.extend(page.warnings);
                    output.warn(
                        rel,
                        "another page of the snapshot has the same id; this one is skipped",
                    );
                } else {
                    output.extend(page);
                }
            }
            Err(error) => output.warn(rel, error.to_string()),
        }
    }
    for orphan in sidecars {
        output.warn(orphan, "comment sidecar without a page; ignored");
    }
    Ok(output)
}

fn relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read_limited(path: &Path, limit: u64) -> std::result::Result<Vec<u8>, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("cannot read: {error}"))?
        .len();
    if size > limit {
        return Err(format!(
            "file is {size} bytes, over the limit of {limit}; skipped"
        ));
    }
    std::fs::read(path).map_err(|error| format!("cannot read: {error}"))
}

/// Ingests a single page: the entry point for incremental re-ingestion.
///
/// `rel_path` is the page's path relative to the snapshot root (it decides the format and the
/// default page id), `bytes` its content and `sidecar` the content of its `.comments.json` file
/// when there is one. On success the output holds exactly one canonical unit, plus warnings for
/// whatever was ingested partially (malformed front matter or sidecar, unknown statuses, duplicate
/// thread or comment ids, dangling `reply_to`), each located at `rel_path`.
///
/// # Errors
/// [`Error::Invalid`] when the extension is not a page format, [`Error::Parse`] when the page is
/// not UTF-8.
pub fn parse_wiki_page(
    source: &SourceId,
    rel_path: &str,
    bytes: &[u8],
    sidecar: Option<&[u8]>,
    options: &WikiOptions,
) -> Result<IngestOutput> {
    let rel = keys::normalize_path(rel_path);
    let format = page_format(&rel)
        .ok_or_else(|| Error::invalid(format!("{rel}: not a wiki page (expected .md or .html)")))?;
    let content = std::str::from_utf8(bytes)
        .map_err(|error| Error::parse(format!("{rel}: page is not valid UTF-8 ({error})")))?;
    let mut problems = Vec::new();
    let mut page = match format {
        PageFormat::Markdown => parse_markdown(content, &mut problems),
        PageFormat::Html => parse_html(content),
    };
    if let Some(sidecar) = sidecar {
        match threads::parse_sidecar(sidecar, &mut problems) {
            Ok(mut from_sidecar) => {
                // Sidecar threads come first so they win over embedded duplicates.
                from_sidecar.append(&mut page.threads);
                page.threads = from_sidecar;
            }
            Err(message) => problems.push(message),
        }
    }
    let unit = build_unit(source, &rel, page, options, &mut problems);
    Ok(IngestOutput {
        units: vec![unit],
        warnings: problems
            .into_iter()
            .map(|message| IngestWarning::new(rel.as_str(), message))
            .collect(),
    })
}

/// Page metadata, from front matter or `<meta>` tags.
struct PageMeta {
    id: Option<String>,
    title: Option<String>,
    parent: Option<String>,
    parent_id: Option<String>,
    labels: Vec<String>,
    author: PersonIdentity,
    created: Option<String>,
    updated: Option<String>,
    url: Option<String>,
    space: Option<String>,
    version: Option<String>,
    documents: Vec<String>,
}

impl PageMeta {
    /// Reads metadata from an object using the front-matter key names.
    fn from_value(value: &Value) -> Self {
        let parent = value.get("parent");
        let nested_parent = parent.filter(|parent| parent.is_object());
        Self {
            id: field(value, &["id", "page_id"]),
            title: field(value, &["title"]),
            parent: nested_parent
                .and_then(|parent| field(parent, &["title", "name"]))
                .or_else(|| field(value, &["parent", "parent_title"])),
            parent_id: field(value, &["parent_id"])
                .or_else(|| nested_parent.and_then(|parent| field(parent, &["id"]))),
            labels: list_field(value, &["labels", "tags"]),
            author: person(value.get("author")),
            created: field(value, &["created", "created_at", "date"]),
            updated: field(value, &["updated", "updated_at", "modified"]),
            url: field(value, &["url"]),
            space: field(value, &["space"]),
            version: field(value, &["version"]),
            documents: list_field(value, &["documents"]),
        }
    }
}

/// A page reduced to what the unit builder needs, whatever its format.
struct ParsedPage {
    meta: PageMeta,
    /// Title found in the content (`<title>`, first `<h1>`), used when the metadata has none.
    content_title: Option<String>,
    /// Markdown-flavoured body text.
    body: String,
    links: Vec<DocLink>,
    threads: Vec<RawThread>,
}

fn parse_markdown(content: &str, problems: &mut Vec<String>) -> ParsedPage {
    let (front_matter, body) = text::split_front_matter(content);
    let mut meta = Value::Null;
    if let Some(yaml) = front_matter {
        match serde_yaml_ng::from_str::<Value>(yaml) {
            Ok(value @ (Value::Object(_) | Value::Null)) => meta = value,
            Ok(_) => problems.push("front matter is not a mapping; ignored".to_owned()),
            Err(error) => problems.push(format!("malformed front matter ignored: {error}")),
        }
    }
    ParsedPage {
        meta: PageMeta::from_value(&meta),
        content_title: None,
        body: body.to_owned(),
        links: text::markdown_links(body),
        threads: Vec::new(),
    }
}

fn flat_text(element: &ElementRef<'_>) -> Option<String> {
    let joined: String = element.text().collect();
    let flat = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    (!flat.is_empty()).then_some(flat)
}

fn parse_html(content: &str) -> ParsedPage {
    let document = Html::parse_document(content);
    let mut metas = serde_json::Map::new();
    let mut title_tag = None;
    let mut first_h1 = None;
    let mut canonical = None;
    let (mut main, mut main_content, mut body) = (None, None, None);
    let mut containers = Vec::new();
    for element in document.root_element().descendent_elements() {
        let value = element.value();
        match value.name() {
            "title" if title_tag.is_none() => title_tag = flat_text(&element),
            "h1" if first_h1.is_none() => first_h1 = flat_text(&element),
            "meta" => {
                let name = value
                    .attr("name")
                    .or_else(|| value.attr("property"))
                    .or_else(|| value.attr("itemprop"));
                if let (Some(name), Some(content)) = (name, value.attr("content")) {
                    metas
                        .entry(name.trim().to_ascii_lowercase())
                        .or_insert_with(|| Value::from(content));
                }
            }
            "link" if canonical.is_none() => {
                if value
                    .attr("rel")
                    .is_some_and(|rel| rel.eq_ignore_ascii_case("canonical"))
                {
                    canonical = value.attr("href").map(str::to_owned);
                }
            }
            "main" if main.is_none() => main = Some(element),
            "body" if body.is_none() => body = Some(element),
            _ => {}
        }
        if main_content.is_none() && value.id() == Some("main-content") {
            main_content = Some(element);
        }
        if threads::is_thread_container(&element)
            && !element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|ancestor| threads::is_thread_container(&ancestor))
        {
            containers.push(element);
        }
    }
    let metas = Value::Object(metas);
    let meta = PageMeta {
        id: field(
            &metas,
            &["page-id", "id", "ajs-page-id", "confluence-page-id"],
        ),
        title: field(&metas, &["title", "ajs-page-title", "og:title", "dc.title"]),
        parent: field(&metas, &["parent", "parent-title", "ajs-parent-page-title"]),
        parent_id: field(&metas, &["parent-id", "ajs-parent-page-id"]),
        labels: list_field(&metas, &["labels", "keywords", "tags", "ajs-labels"]),
        author: person(
            ["author", "creator", "ajs-creator-name", "dc.creator"]
                .iter()
                .find_map(|name| metas.get(name)),
        ),
        created: field(
            &metas,
            &[
                "created",
                "date",
                "dcterms.created",
                "dc.date.created",
                "article:published_time",
            ],
        ),
        updated: field(
            &metas,
            &[
                "updated",
                "modified",
                "last-modified",
                "dcterms.modified",
                "dc.date.modified",
                "article:modified_time",
            ],
        ),
        url: field(&metas, &["url", "canonical", "og:url"]).or(canonical),
        space: field(&metas, &["space", "space-key", "ajs-space-key"]),
        version: field(&metas, &["version", "ajs-page-version"]),
        documents: list_field(&metas, &["documents"]),
    };
    let root = main
        .or(main_content)
        .or(body)
        .unwrap_or_else(|| document.root_element());
    let extracted = text::element_text(root, &|element| threads::is_thread_container(element));
    ParsedPage {
        meta,
        content_title: title_tag.or(first_h1),
        body: extracted.text,
        links: extracted.links,
        threads: containers.into_iter().map(threads::html_thread).collect(),
    }
}

/// Parses the timestamp shapes wiki exports use into milliseconds since the Unix epoch.
fn timestamp(text: &str) -> Option<i64> {
    let text = text.trim();
    parse_timestamp_ms(text).or_else(|| {
        let bytes = text.as_bytes();
        let dated = bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' && text.is_ascii();
        if !dated {
            return None;
        }
        match text.len() {
            10 => parse_timestamp_ms(&format!("{text}T00:00:00Z")),
            16 => parse_timestamp_ms(&format!("{}T{}:00Z", &text[..10], &text[11..])),
            19 => parse_timestamp_ms(&format!("{}T{}Z", &text[..10], &text[11..])),
            _ => None,
        }
    })
}

/// An id for something the source did not name, derived from what identifies it.
fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    format!("{prefix}-{}", &hasher.finalize().to_hex()[..16])
}

fn normalize_status(raw: Option<&str>) -> (&'static str, bool) {
    let Some(raw) = raw else {
        return (attrs::THREAD_OPEN, true);
    };
    match keys::slugify(raw).as_str() {
        "open" | "active" | "unresolved" | "reopened" | "pending" => (attrs::THREAD_OPEN, true),
        "resolved" | "closed" | "done" | "fixed" | "accepted" | "addressed" => {
            (attrs::THREAD_RESOLVED, true)
        }
        "dismissed" | "rejected" | "declined" | "wontfix" | "won-t-fix" | "outdated"
        | "obsolete" => (attrs::THREAD_DISMISSED, true),
        _ => (attrs::THREAD_OPEN, false),
    }
}

fn normalize_kind(raw: Option<&str>, has_anchor: bool) -> &'static str {
    match raw.map(keys::slugify).as_deref() {
        Some("page" | "footer" | "general") => "page",
        Some("inline" | "annotation") => "inline",
        Some("review" | "code-review") => "review",
        _ if has_anchor => "inline",
        _ => "page",
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut short: String = flat.chars().take(max_chars).collect();
    short.push('…');
    short
}

/// Asserts `identity` as a Person and returns its key and display name.
fn assert_person(unit: &mut Unit, identity: &PersonIdentity) -> Option<(NodeKey, String)> {
    let node = identity.assertion()?;
    let name = node.name.clone();
    Some((unit.add_node(node), name))
}

fn title_candidates(title: &str) -> Vec<String> {
    let mut candidates = vec![title.to_owned()];
    let mut words: Vec<&str> = title.split_whitespace().collect();
    while words.len() > 1
        && words.last().is_some_and(|word| {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric());
            TITLE_SUFFIXES.contains(&word.to_lowercase().as_str())
        })
    {
        words.pop();
    }
    let stripped = words.join(" ");
    if stripped != title && !stripped.is_empty() {
        candidates.push(stripped);
    }
    candidates
}

fn build_unit(
    source: &SourceId,
    rel: &str,
    page: ParsedPage,
    options: &WikiOptions,
    problems: &mut Vec<String>,
) -> Unit {
    let ParsedPage {
        meta,
        content_title,
        body,
        links,
        threads,
    } = page;
    let stem_path = text::strip_extension(rel);
    let page_id = meta.id.clone().unwrap_or_else(|| {
        let slug = keys::slugify(stem_path);
        if slug.is_empty() {
            stable_id("page", &[rel])
        } else {
            slug
        }
    });
    let sections = text::markdown_sections(&body);
    let title = meta
        .title
        .clone()
        .or(content_title)
        .or_else(|| {
            sections
                .iter()
                .find(|section| section.level == 1)
                .and_then(|section| section.heading_path.first().cloned())
                .filter(|heading| !heading.is_empty())
        })
        .unwrap_or_else(|| stem_path.rsplit('/').next().unwrap_or(stem_path).to_owned());

    let mut unit = Unit::new(
        keys::unit(source, UnitKind::WikiPage, &page_id),
        source.clone(),
        UnitKind::WikiPage,
    );
    let mut node = NodeAssertion::new(
        NodeKind::WikiPage,
        keys::wiki_page(source, &page_id),
        title.as_str(),
    )
    .alias(title.as_str())
    .alias(keys::slugify(&title))
    .alias(page_id.as_str())
    .alias(stem_path)
    .attr(attrs::TITLE, title.as_str())
    .attr(attrs::PATH, rel)
    .attr_opt(attrs::URL, meta.url.clone())
    .attr_opt(attrs::VERSION, meta.version.clone())
    .attr_opt(ATTR_SPACE, meta.space.clone())
    .attr_opt(
        attrs::CREATED_AT,
        meta.created.as_deref().and_then(timestamp),
    )
    .attr_opt(
        attrs::UPDATED_AT,
        meta.updated.as_deref().and_then(timestamp),
    );
    if !meta.labels.is_empty() {
        node = node.attr(attrs::LABELS, meta.labels.clone());
    }
    let page_key = unit.add_node(node);

    if let Some(parent_id) = &meta.parent_id {
        unit.add_edge(EdgeAssertion::new(
            keys::wiki_page(source, parent_id),
            EdgeKind::Contains,
            &page_key,
        ));
    } else if let Some(parent) = &meta.parent {
        unit.add_edge(EdgeAssertion::new(
            Lookup::new(&[NodeKind::WikiPage], parent.as_str()).in_source(source.clone()),
            EdgeKind::Contains,
            &page_key,
        ));
    }
    if let Some((author, _)) = assert_person(&mut unit, &meta.author) {
        unit.add_edge(EdgeAssertion::new(&page_key, EdgeKind::AuthoredBy, author));
    }

    for chunk in text::chunk_sections(&sections, options.max_chunk_chars) {
        unit.add_chunk(&page_key, ChunkKind::Text, chunk);
    }
    for edge in mentions::mention_edges(&page_key, &format!("{title}\n{body}")) {
        unit.add_edge(edge);
    }

    let mut linked = BTreeSet::new();
    for link in &links {
        let target = match link.kind {
            LinkKind::WikiTitle => Some(link.target.clone()),
            LinkKind::Path | LinkKind::Doc => text::resolve_relative(rel, &link.target)
                .map(|resolved| text::strip_extension(&resolved).to_owned()),
        };
        linked.extend(target.filter(|target| target != stem_path));
    }
    for target in linked {
        unit.add_edge(
            EdgeAssertion::new(
                &page_key,
                EdgeKind::Mentions,
                Lookup::new(&[NodeKind::WikiPage], target).in_source(source.clone()),
            )
            .weight(PAGE_LINK_WEIGHT),
        );
    }

    for name in &meta.documents {
        unit.add_edge(
            EdgeAssertion::new(
                &page_key,
                EdgeKind::Documents,
                Lookup::new(DOCUMENTED_KINDS, name.as_str()),
            )
            .weight(DOCUMENTS_WEIGHT),
        );
    }
    for candidate in title_candidates(&title) {
        unit.add_edge(
            EdgeAssertion::new(
                &page_key,
                EdgeKind::Documents,
                Lookup::new(TITLE_KINDS, candidate),
            )
            .weight(DOCUMENTS_WEIGHT),
        );
    }

    let mut seen_threads = HashSet::new();
    for (ordinal, thread) in threads.into_iter().enumerate() {
        add_thread(
            &mut unit,
            &page_key,
            &page_id,
            &title,
            ordinal,
            thread,
            &mut seen_threads,
            problems,
        );
    }
    unit.finish()
}

#[expect(
    clippy::too_many_arguments,
    reason = "the page context a thread is built in is passed explicitly rather than bundled into a one-use struct"
)]
fn add_thread(
    unit: &mut Unit,
    page_key: &NodeKey,
    page_id: &str,
    page_title: &str,
    ordinal: usize,
    thread: RawThread,
    seen_threads: &mut HashSet<String>,
    problems: &mut Vec<String>,
) {
    let source = unit.source.clone();
    let anchor = thread.anchor.as_deref().unwrap_or("");
    if thread.comments.is_empty() && anchor.is_empty() && thread.close_reason.is_none() {
        problems.push(format!("thread #{ordinal} is empty; skipped"));
        return;
    }
    let first = thread.comments.first();
    let thread_id = thread.id.clone().unwrap_or_else(|| {
        stable_id(
            "t",
            &[
                page_id,
                &ordinal.to_string(),
                anchor,
                first
                    .and_then(|comment| comment.author.display_name())
                    .unwrap_or(""),
                first
                    .and_then(|comment| comment.created.as_deref())
                    .unwrap_or(""),
            ],
        )
    });
    if !seen_threads.insert(thread_id.clone()) {
        problems.push(format!(
            "duplicate thread id {thread_id:?}; later copy skipped"
        ));
        return;
    }
    let (status, known) = normalize_status(thread.status.as_deref());
    if !known {
        problems.push(format!(
            "thread {thread_id:?} has unknown status {:?}; treated as open",
            thread.status.as_deref().unwrap_or("")
        ));
    }
    let kind = normalize_kind(thread.kind.as_deref(), !anchor.is_empty());
    let closer = assert_person(unit, &thread.closed_by);
    let closed_at = thread.closed_at.as_deref().and_then(timestamp);
    let comment_times: Vec<i64> = thread
        .comments
        .iter()
        .filter_map(|comment| comment.created.as_deref().and_then(timestamp))
        .collect();
    let created_at = comment_times.iter().copied().min();
    let updated_at = comment_times.iter().copied().chain(closed_at).max();

    let name = if anchor.is_empty() {
        format!("Thread on {page_title}")
    } else {
        format!("Thread on \"{}\" ({page_title})", truncate(anchor, 80))
    };
    let thread_key = unit.add_node(
        NodeAssertion::new(
            NodeKind::Thread,
            keys::thread(&source, &format!("{page_id}/{thread_id}")),
            name,
        )
        .attr(attrs::THREAD_STATUS, status)
        .attr(attrs::THREAD_KIND, kind)
        .attr_opt(attrs::ANCHOR_TEXT, thread.anchor.clone())
        .attr_opt(
            attrs::CLOSED_BY,
            closer.as_ref().map(|(_, name)| name.clone()),
        )
        .attr_opt(attrs::CLOSE_REASON, thread.close_reason.clone())
        .attr_opt(attrs::CREATED_AT, created_at)
        .attr_opt(attrs::UPDATED_AT, updated_at)
        .attr_opt(ATTR_CLOSED_AT, closed_at),
    );
    unit.add_edge(EdgeAssertion::new(
        page_key,
        EdgeKind::Contains,
        &thread_key,
    ));
    if let Some((closer_key, _)) = &closer {
        unit.add_edge(EdgeAssertion::new(
            &thread_key,
            EdgeKind::DecidedBy,
            closer_key,
        ));
    }
    if let Some(reason) = &thread.close_reason {
        let verb = match status {
            attrs::THREAD_DISMISSED => "Dismissed",
            attrs::THREAD_RESOLVED => "Resolved",
            _ => "Noted",
        };
        let decision = match &closer {
            Some((_, name)) => format!("{verb} by {name}: {reason}"),
            None => format!("{verb}: {reason}"),
        };
        unit.add_chunk(&thread_key, ChunkKind::Decision, decision);
    }
    let discussed = format!(
        "{anchor}\n{}",
        first.map_or("", |comment| comment.body.as_str())
    );
    for mention in mentions::extract_mentions(&discussed) {
        unit.add_edge(
            EdgeAssertion::new(&thread_key, EdgeKind::Discusses, mention.lookup)
                .weight(mention.weight),
        );
    }

    // First pass: every comment gets its key, so replies may point forwards as well as back.
    let mut comment_keys: Vec<Option<NodeKey>> = Vec::with_capacity(thread.comments.len());
    let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
    for (position, comment) in thread.comments.iter().enumerate() {
        let comment_id = comment.id.clone().unwrap_or_else(|| {
            stable_id(
                "c",
                &[
                    page_id,
                    &thread_id,
                    &position.to_string(),
                    comment.author.display_name().unwrap_or(""),
                    comment.created.as_deref().unwrap_or(""),
                ],
            )
        });
        if by_id.contains_key(&comment_id) {
            problems.push(format!(
                "duplicate comment id {comment_id:?} in thread {thread_id:?}; later copy skipped"
            ));
            comment_keys.push(None);
            continue;
        }
        comment_keys.push(Some(keys::comment(
            &source,
            &format!("{page_id}/{thread_id}/{comment_id}"),
        )));
        by_id.insert(comment_id, position);
    }
    for (position, comment) in thread.comments.iter().enumerate() {
        let Some(comment_key) = comment_keys[position].clone() else {
            continue;
        };
        let author = assert_person(unit, &comment.author);
        let name = match &author {
            Some((_, author)) => format!("Comment by {author} on {page_title}"),
            None => format!("Comment on {page_title}"),
        };
        unit.add_node(
            NodeAssertion::new(NodeKind::Comment, comment_key.clone(), name).attr_opt(
                attrs::CREATED_AT,
                comment.created.as_deref().and_then(timestamp),
            ),
        );
        unit.add_edge(EdgeAssertion::new(
            &thread_key,
            EdgeKind::Contains,
            &comment_key,
        ));
        if let Some((author_key, _)) = author {
            unit.add_edge(EdgeAssertion::new(
                &comment_key,
                EdgeKind::AuthoredBy,
                author_key,
            ));
        }
        let replied = match &comment.reply_to {
            Some(target) => {
                let found = by_id.get(target).copied();
                if found.is_none() {
                    problems.push(format!(
                        "comment replies to unknown comment {target:?} in thread {thread_id:?}"
                    ));
                }
                found
            }
            None => comment.parent,
        };
        if let Some(parent_key) = replied
            .filter(|parent| *parent != position)
            .and_then(|parent| comment_keys.get(parent).cloned().flatten())
        {
            unit.add_edge(EdgeAssertion::new(
                &comment_key,
                EdgeKind::ReplyTo,
                parent_key,
            ));
        }
        unit.add_chunk(&comment_key, ChunkKind::Comment, comment.body.as_str());
        for edge in mentions::mention_edges(&comment_key, &comment.body) {
            unit.add_edge(edge);
        }
    }
}
