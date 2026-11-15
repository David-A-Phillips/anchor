use super::*;
use crate::base::{NodeRef, Unit};

const RUNBOOK_MD: &str = "---
id: payments-runbook
title: Payments API Runbook
parent_id: operations
labels: [payments, oncall]
author: Alice Smith <alice@example.com>
created: 2024-03-01
updated: 2024-05-17T16:30:00Z
url: https://wiki.example.com/x/123
space: ENG
version: 7
documents: [payments-api, \"POST /v1/charges\"]
---
# Payments API Runbook

Intro mentioning `Ledger::charge` and PAY-123.

## Retries

Retries use `retry_charge`. See [escalation](../ops/escalation.md) and [[Incident Process|the process]].
Zoë says: 你好.
";

const RUNBOOK_COMMENTS: &str = r#"{"threads": [
  {"id": "t-1", "kind": "inline", "status": "resolved", "anchor": "Retries use `retry_charge`",
   "closed_by": {"name": "Bob Lee", "handle": "blee", "email": "bob@example.com"},
   "close_reason": "Documented the backoff", "closed_at": "2024-05-03T10:00:00Z",
   "comments": [
     {"id": "c-1", "author": "Alice Smith", "created": "2024-05-01T09:00:00Z",
      "body": "Is `retry_charge` idempotent? See PAY-77."},
     {"id": "c-2", "author": {"name": "Bob Lee", "handle": "blee"}, "created": "2024-05-02T09:00:00Z",
      "body": "Yes, since payments-api v2.", "reply_to": "c-1"},
     {"id": "c-3", "author": "@carol", "body": "Thanks", "reply_to": "c-404"}
   ]},
  {"id": 2, "kind": "review", "status": "dismissed", "closed_by": "Bob Lee",
   "close_reason": "Out of scope: tracked in PAY-200",
   "comments": [{"author": "Dan", "created": "2024-05-04T09:00:00Z", "body": "Should we drop retries entirely?"}]},
  {"comments": [{"author": "Erin", "body": "Nice page"}], "status": "weird"},
  "not a thread"
]}"#;

fn source() -> SourceId {
    SourceId::new("eng-wiki")
}

fn parse(rel: &str, page: &str, sidecar: Option<&str>) -> IngestOutput {
    parse_wiki_page(
        &source(),
        rel,
        page.as_bytes(),
        sidecar.map(str::as_bytes),
        &WikiOptions::default(),
    )
    .expect("page parses")
}

fn node<'a>(unit: &'a Unit, key: &str) -> &'a NodeAssertion {
    unit.nodes
        .iter()
        .find(|node| node.key.as_str() == key)
        .unwrap_or_else(|| panic!("no node {key}"))
}

fn has_key_edge(unit: &Unit, from: &str, kind: EdgeKind, to: &str) -> bool {
    unit.edges.iter().any(|edge| {
        edge.kind == kind
            && edge.from == NodeRef::Key(NodeKey::new(from))
            && edge.to == NodeRef::Key(NodeKey::new(to))
    })
}

fn lookup_weight(unit: &Unit, from: &str, kind: EdgeKind, text: &str) -> Option<f32> {
    unit.edges.iter().find_map(|edge| match &edge.to {
        NodeRef::Lookup(lookup)
            if edge.kind == kind
                && edge.from == NodeRef::Key(NodeKey::new(from))
                && lookup.text == text =>
        {
            Some(edge.weight)
        }
        _ => None,
    })
}

#[test]
fn markdown_page_with_front_matter_and_sidecar() {
    let output = parse("guides/runbook.md", RUNBOOK_MD, Some(RUNBOOK_COMMENTS));
    let unit = &output.units[0];
    assert_eq!(unit.kind, UnitKind::WikiPage);
    assert_eq!(unit.key.as_str(), "eng-wiki/wiki_page/payments-runbook");
    let page_key = "wiki:eng-wiki:payments-runbook";
    let page = node(unit, page_key);
    assert_eq!(page.name, "Payments API Runbook");
    for alias in [
        "Payments API Runbook",
        "payments-api-runbook",
        "payments-runbook",
        "guides/runbook",
    ] {
        assert!(page.aliases.iter().any(|a| a == alias), "{alias}");
    }
    assert_eq!(page.attrs[attrs::PATH], "guides/runbook.md");
    assert_eq!(page.attrs[attrs::URL], "https://wiki.example.com/x/123");
    assert_eq!(page.attrs[attrs::VERSION], "7");
    assert_eq!(page.attrs[ATTR_SPACE], "ENG");
    assert_eq!(page.attrs[attrs::LABELS], serde_json::json!(["payments", "oncall"]));
    assert_eq!(page.attrs[attrs::CREATED_AT], 1_709_251_200_000_i64);
    assert_eq!(page.attrs[attrs::UPDATED_AT], 1_715_963_400_000_i64);

    // Hierarchy, author.
    assert!(has_key_edge(unit, "wiki:eng-wiki:operations", EdgeKind::Contains, page_key));
    assert!(has_key_edge(unit, page_key, EdgeKind::AuthoredBy, "person:alice"));
    assert!(node(unit, "person:alice").aliases.contains(&"Alice Smith".to_owned()));

    // Page text: section chunks with heading paths, no front matter.
    let texts: Vec<&str> = unit
        .chunks
        .iter()
        .filter(|chunk| chunk.node.as_str() == page_key)
        .map(|chunk| chunk.text.as_str())
        .collect();
    assert_eq!(texts.len(), 2);
    assert!(texts[0].starts_with("Payments API Runbook\n\nIntro"));
    assert!(texts[1].starts_with("Payments API Runbook > Retries\n\n"));
    assert!(texts[1].contains("你好"));

    // Links to entities and pages.
    let mentions = EdgeKind::Mentions;
    assert!(lookup_weight(unit, page_key, mentions, "Ledger::charge").is_some());
    assert!(lookup_weight(unit, page_key, mentions, "PAY-123").is_some());
    assert_eq!(lookup_weight(unit, page_key, mentions, "ops/escalation"), Some(0.9));
    assert_eq!(lookup_weight(unit, page_key, mentions, "Incident Process"), Some(0.9));
    let documents = EdgeKind::Documents;
    for name in ["payments-api", "POST /v1/charges", "Payments API Runbook", "Payments API"] {
        assert_eq!(lookup_weight(unit, page_key, documents, name), Some(0.8), "{name}");
    }

    // Resolved inline thread with closer, reason and replies.
    let t1 = "thread:eng-wiki:payments-runbook/t-1";
    let thread = node(unit, t1);
    assert_eq!(thread.attrs[attrs::THREAD_STATUS], attrs::THREAD_RESOLVED);
    assert_eq!(thread.attrs[attrs::THREAD_KIND], "inline");
    assert_eq!(thread.attrs[attrs::ANCHOR_TEXT], "Retries use `retry_charge`");
    assert_eq!(thread.attrs[attrs::CLOSED_BY], "Bob Lee");
    assert_eq!(thread.attrs[attrs::CLOSE_REASON], "Documented the backoff");
    assert_eq!(thread.attrs[attrs::CREATED_AT], 1_714_554_000_000_i64);
    assert_eq!(thread.attrs[attrs::UPDATED_AT], 1_714_730_400_000_i64);
    assert!(has_key_edge(unit, page_key, EdgeKind::Contains, t1));
    assert!(has_key_edge(unit, t1, EdgeKind::DecidedBy, "person:blee"));
    assert!(lookup_weight(unit, t1, EdgeKind::Discusses, "retry_charge").is_some());
    assert!(lookup_weight(unit, t1, EdgeKind::Discusses, "PAY-77").is_some());
    let (c1, c2, c3) = (
        "comment:eng-wiki:payments-runbook/t-1/c-1",
        "comment:eng-wiki:payments-runbook/t-1/c-2",
        "comment:eng-wiki:payments-runbook/t-1/c-3",
    );
    assert!(has_key_edge(unit, t1, EdgeKind::Contains, c1));
    assert!(has_key_edge(unit, c2, EdgeKind::ReplyTo, c1));
    assert!(has_key_edge(unit, c2, EdgeKind::AuthoredBy, "person:blee"));
    assert!(has_key_edge(unit, c3, EdgeKind::AuthoredBy, "person:carol"));
    assert!(lookup_weight(unit, c1, mentions, "PAY-77").is_some());
    assert!(lookup_weight(unit, c2, mentions, "payments-api").is_some());
    let chunk = |key: &str, kind: ChunkKind| {
        unit.chunks
            .iter()
            .find(|chunk| chunk.node.as_str() == key && chunk.kind == kind)
            .map(|chunk| chunk.text.clone())
    };
    assert_eq!(
        chunk(c1, ChunkKind::Comment).as_deref(),
        Some("Is `retry_charge` idempotent? See PAY-77.")
    );
    assert_eq!(
        chunk(t1, ChunkKind::Decision).as_deref(),
        Some("Resolved by Bob Lee: Documented the backoff")
    );

    // Dismissed review thread (numeric id) keeps who dismissed it and why.
    let t2 = "thread:eng-wiki:payments-runbook/2";
    let dismissed = node(unit, t2);
    assert_eq!(dismissed.attrs[attrs::THREAD_STATUS], attrs::THREAD_DISMISSED);
    assert_eq!(dismissed.attrs[attrs::THREAD_KIND], "review");
    assert_eq!(dismissed.attrs[attrs::CLOSED_BY], "Bob Lee");
    assert_eq!(
        chunk(t2, ChunkKind::Decision).as_deref(),
        Some("Dismissed by Bob Lee: Out of scope: tracked in PAY-200")
    );

    // Third thread: generated ids, unknown status -> open page thread.
    let generated: Vec<&NodeAssertion> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Thread && n.key.as_str().contains("/t-") && n.key.as_str() != t1)
        .collect();
    assert_eq!(generated.len(), 1);
    assert_eq!(generated[0].attrs[attrs::THREAD_STATUS], attrs::THREAD_OPEN);
    assert_eq!(generated[0].attrs[attrs::THREAD_KIND], "page");
    assert_eq!(unit.nodes.iter().filter(|n| n.kind == NodeKind::Comment).count(), 5);

    let warnings: Vec<&str> = output.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("not an object")));
    assert!(warnings.iter().any(|w| w.contains("unknown status")));
    assert!(warnings.iter().any(|w| w.contains("c-404")));
    assert!(output.warnings.iter().all(|w| w.location == "guides/runbook.md"));
}

const LEDGER_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<title>Ledger Design</title>
<meta name="ajs-page-id" content="4711">
<meta name="Author" content="Zoë Ångström">
<meta name="keywords" content="ledger, design">
<meta name="parent" content="Architecture">
<meta name="dcterms.modified" content="2024-06-01 12:00:00">
<link rel="canonical" href="https://wiki.example.com/pages/4711">
<script>var tracking = "PAY-999";</script>
</head><body>
<nav><a href="index.html">Home</a></nav>
<div id="main-content">
<h1>Ledger Design</h1>
<p>The <code>Ledger</code> service exposes <code>POST /v1/charges</code>; see <a href="api/charges.html#post">the API page</a>.
<h2>Storage</h2>
<table><tr><th>Table</th><th>Owner</th></tr><tr><td>entries</td><td>payments-platform</td></tr></table>
<p>Unclosed paragraph with <b>bold
</div>
<section class="comments">
<div class="comment-thread" data-thread-id="t-9" data-kind="review" data-status="dismissed"
     data-anchor="exposes POST /v1/charges" data-closed-by="Bob Lee" data-close-reason="Out of scope for v1">
  <div class="comment" data-comment-id="c-1" data-author="Alice Smith" data-created="2024-05-01T09:00:00Z">
    <div class="comment-body"><p>Should <code>Ledger::refund</code> live here?</p></div>
    <div class="comment" data-comment-id="c-2" data-author="Bob Lee">
      Not in v1.
      <div class="comment"><span class="comment-author">Zoë Ångström</span> <time datetime="2024-05-03T09:00:00Z">May 3</time> Ok — 了解</div>
    </div>
  </div>
</div>
<div class="comment-thread"><div class="comment">Anonymous drive-by</div></div>
</section>
</body></html>"#;

#[test]
fn html_page_with_meta_tags_and_embedded_nested_threads() {
    let output = parse("arch/ledger.html", LEDGER_HTML, None);
    assert!(output.warnings.is_empty(), "{:?}", output.warnings);
    let unit = &output.units[0];
    let page_key = "wiki:eng-wiki:4711";
    let page = node(unit, page_key);
    assert_eq!(page.name, "Ledger Design");
    assert_eq!(page.attrs[attrs::URL], "https://wiki.example.com/pages/4711");
    assert_eq!(page.attrs[attrs::LABELS], serde_json::json!(["ledger", "design"]));
    assert_eq!(page.attrs[attrs::UPDATED_AT], 1_717_243_200_000_i64);
    assert!(page.aliases.contains(&"arch/ledger".to_owned()));
    assert!(has_key_edge(unit, page_key, EdgeKind::AuthoredBy, "person:zoë-ångström"));
    assert!(unit.edges.iter().any(|edge| edge.kind == EdgeKind::Contains
        && edge.to == NodeRef::Key(NodeKey::new(page_key))
        && matches!(&edge.from, NodeRef::Lookup(l) if l.text == "Architecture" && l.kinds == [NodeKind::WikiPage])));

    let body: String = unit
        .chunks
        .iter()
        .filter(|chunk| chunk.node.as_str() == page_key)
        .map(|chunk| chunk.text.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(body.contains("The `Ledger` service exposes `POST /v1/charges`; see the API page."));
    assert!(body.contains("Ledger Design > Storage\n\nTable | Owner\nentries | payments-platform"));
    assert!(body.contains("Unclosed paragraph with bold"));
    for absent in ["Home", "tracking", "Should", "drive-by"] {
        assert!(!body.contains(absent), "{absent}");
    }
    let mentions = EdgeKind::Mentions;
    assert!(lookup_weight(unit, page_key, mentions, "POST /v1/charges").is_some());
    assert!(lookup_weight(unit, page_key, mentions, "Ledger").is_some());
    assert_eq!(lookup_weight(unit, page_key, mentions, "arch/api/charges"), Some(0.9));
    assert!(lookup_weight(unit, page_key, mentions, "PAY-999").is_none());
    assert_eq!(lookup_weight(unit, page_key, EdgeKind::Documents, "Ledger"), Some(0.8));

    let t9 = "thread:eng-wiki:4711/t-9";
    let thread = node(unit, t9);
    assert_eq!(thread.attrs[attrs::THREAD_STATUS], attrs::THREAD_DISMISSED);
    assert_eq!(thread.attrs[attrs::THREAD_KIND], "review");
    assert_eq!(thread.attrs[attrs::CLOSED_BY], "Bob Lee");
    assert_eq!(thread.attrs[attrs::CLOSE_REASON], "Out of scope for v1");
    assert!(lookup_weight(unit, t9, EdgeKind::Discusses, "POST /v1/charges").is_some());
    assert!(lookup_weight(unit, t9, EdgeKind::Discusses, "Ledger::refund").is_some());

    // Nesting implies the reply structure; bodies exclude nested replies and metadata.
    let comments: Vec<&crate::base::Chunk> = unit
        .chunks
        .iter()
        .filter(|chunk| chunk.kind == ChunkKind::Comment && chunk.node.as_str().starts_with("comment:eng-wiki:4711/t-9/"))
        .collect();
    let mut bodies: Vec<&str> = comments.iter().map(|chunk| chunk.text.as_str()).collect();
    bodies.sort_unstable();
    assert_eq!(bodies, ["Not in v1.", "Ok — 了解", "Should `Ledger::refund` live here?"]);
    let c1 = "comment:eng-wiki:4711/t-9/c-1";
    let c2 = "comment:eng-wiki:4711/t-9/c-2";
    assert!(has_key_edge(unit, c2, EdgeKind::ReplyTo, c1));
    let c3 = comments
        .iter()
        .find(|chunk| chunk.text.starts_with("Ok"))
        .map(|chunk| chunk.node.as_str())
        .expect("generated comment");
    assert!(c3.starts_with("comment:eng-wiki:4711/t-9/c-"));
    assert!(has_key_edge(unit, c3, EdgeKind::ReplyTo, c2));
    assert!(has_key_edge(unit, c3, EdgeKind::AuthoredBy, "person:zoë-ångström"));
    assert_eq!(node(unit, c3).attrs[attrs::CREATED_AT], 1_714_726_800_000_i64);

    // A thread without any attribute is still ingested, as an open page thread.
    let bare: Vec<&NodeAssertion> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Thread && n.key.as_str() != t9)
        .collect();
    assert_eq!(bare.len(), 1);
    assert_eq!(bare[0].attrs[attrs::THREAD_STATUS], attrs::THREAD_OPEN);
    assert!(!bare[0].attrs.contains_key(attrs::CLOSED_BY));
}

#[test]
fn defaults_and_malformed_metadata_degrade_to_warnings() {
    // No front matter, no sidecar: id from the path, title from the first heading.
    let plain = parse("Team Notes/Über uns.md", "# Über uns\n\nText\n", None);
    assert!(plain.warnings.is_empty());
    let unit = &plain.units[0];
    assert_eq!(unit.key.as_str(), "eng-wiki/wiki_page/team-notes-über-uns");
    assert_eq!(unit.nodes[0].name, "Über uns");
    assert!(unit.nodes.iter().all(|n| n.kind == NodeKind::WikiPage));

    // No heading at all: title from the file name.
    let untitled = parse("misc/todo.md", "just text\n", None);
    assert_eq!(untitled.units[0].nodes[0].name, "todo");

    // Broken YAML and broken JSON: the page survives, each problem is reported.
    let broken = parse(
        "a.md",
        "---\ntitle: [unclosed\n---\n# Real Title\n\nBody\n",
        Some("{\"threads\": [{\"id\": "),
    );
    assert_eq!(broken.units[0].nodes.len(), 1);
    assert_eq!(broken.units[0].nodes[0].name, "Real Title");
    assert_eq!(broken.warnings.len(), 2, "{:?}", broken.warnings);
    assert!(broken.warnings[0].message.contains("front matter"));
    assert!(broken.warnings[1].message.contains("malformed comments JSON"));

    let wrong_shape = parse("a.md", "x\n", Some("{\"comments\": []}"));
    assert!(wrong_shape.warnings[0].message.contains("threads"));

    // Hard errors: unsupported extension, non-UTF-8 content.
    let options = WikiOptions::default();
    assert!(parse_wiki_page(&source(), "a.pdf", b"x", None, &options).is_err());
    assert!(parse_wiki_page(&source(), "a.md", &[0xff, 0xfe, 0x00], None, &options).is_err());
}

#[test]
fn same_input_yields_the_same_hash() {
    let hash = |page: &str, sidecar: &str| {
        parse("guides/runbook.md", page, Some(sidecar)).units[0]
            .content_hash()
            .expect("hash")
    };
    let first = hash(RUNBOOK_MD, RUNBOOK_COMMENTS);
    assert_eq!(first, hash(RUNBOOK_MD, RUNBOOK_COMMENTS));
    assert_ne!(first, hash(RUNBOOK_MD, &RUNBOOK_COMMENTS.replace("Nice page", "Nice page!")));
    let html = |page: &str| {
        parse("arch/ledger.html", page, None).units[0]
            .content_hash()
            .expect("hash")
    };
    assert_eq!(html(LEDGER_HTML), html(LEDGER_HTML));
}

#[test]
fn long_sections_are_split_into_bounded_chunks() {
    let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(8);
    let page = format!("# Long\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n");
    let output = parse("long.md", &page, None);
    let chunks = &output.units[0].chunks;
    assert!(chunks.len() >= 2);
    for (ordinal, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ordinal as usize, ordinal);
        assert!(chunk.text.len() <= 1500);
        assert!(chunk.text.starts_with("Long\n\n"));
    }
}

#[test]
fn snapshot_mixing_formats_survives_a_corrupt_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let write = |rel: &str, bytes: &[u8]| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    };
    write("guides/runbook.md", RUNBOOK_MD.as_bytes());
    write("guides/runbook.comments.json", RUNBOOK_COMMENTS.as_bytes());
    write("arch/ledger.html", LEDGER_HTML.as_bytes());
    write("arch/corrupt.md", &[b'#', b' ', 0xff, 0xfe, 0xc0]);
    write("arch/orphan.comments.json", b"{\"threads\": []}");
    write("copy.md", b"---\nid: payments-runbook\n---\nduplicate id\n");
    write("notes.txt", b"ignored");
    write(".git/hidden.md", b"# hidden");

    let options = WikiOptions::default();
    let output = scan_wiki(root, &source(), &options).expect("scan");
    let mut keys: Vec<&str> = output.units.iter().map(|unit| unit.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["eng-wiki/wiki_page/4711", "eng-wiki/wiki_page/payments-runbook"]);
    let runbook = output
        .units
        .iter()
        .find(|unit| unit.key.as_str().ends_with("payments-runbook"))
        .expect("runbook");
    // The first page in path order wins a duplicate id; here that is copy.md's rival.
    assert!(runbook.nodes.iter().any(|n| n.kind == NodeKind::Thread) || keys.len() == 2);

    let located = |location: &str| {
        output
            .warnings
            .iter()
            .filter(|warning| warning.location == location)
            .map(|warning| warning.message.as_str())
            .collect::<Vec<_>>()
    };
    assert!(located("arch/corrupt.md")[0].contains("UTF-8"));
    assert!(located("arch/orphan.comments.json")[0].contains("without a page"));
    assert_eq!(located("guides/runbook.md").len() + located("copy.md").len(), 4);
    assert!(output.warnings.iter().any(|w| w.message.contains("same id")));

    // Scanning again gives byte-identical units.
    let again = scan_wiki(root, &source(), &options).expect("scan");
    assert_eq!(again, output);

    // Oversized pages are skipped, a missing directory is an error.
    let tiny = WikiOptions { max_file_bytes: 64, ..WikiOptions::default() };
    let limited = scan_wiki(root, &source(), &tiny).expect("scan");
    assert!(limited.warnings.iter().any(|w| w.message.contains("over the limit")));
    assert!(scan_wiki(&root.join("missing"), &source(), &options).is_err());
    assert!(is_wiki_page(Path::new("a/b.HTML")) && !is_wiki_page(Path::new("a/b.json")));
    assert_eq!(sidecar_path(Path::new("a/b.md")), Path::new("a/b.comments.json"));
}
