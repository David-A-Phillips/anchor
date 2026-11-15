use serde_json::{Value, json};

use super::*;
use crate::base::{ChunkKind, EdgeAssertion, EdgeKind, Lookup, NodeKind, NodeRef, UnitKind, keys};

fn source() -> SourceId {
    SourceId::new("jira")
}

fn parse(issue: Value) -> Unit {
    parse_issue(&source(), &issue, &JiraOptions::default()).unwrap()
}

fn key_ref(key: &str) -> NodeRef {
    NodeRef::Key(keys::ticket(key))
}

fn edge<'a>(unit: &'a Unit, from: &NodeRef, kind: EdgeKind, to: &NodeRef) -> &'a EdgeAssertion {
    unit.edges
        .iter()
        .find(|edge| edge.from == *from && edge.kind == kind && edge.to == *to)
        .unwrap_or_else(|| panic!("no edge {from:?} --{kind}--> {to:?} in {:#?}", unit.edges))
}

fn ticket_attr<'a>(unit: &'a Unit, key: &str, attr: &str) -> Option<&'a Value> {
    let ticket = keys::ticket(key);
    unit.nodes
        .iter()
        .find(|node| node.key == ticket)
        .unwrap()
        .attrs
        .get(attr)
}

fn full_issue() -> Value {
    json!({
        "key": "pay-123",
        "self": "https://jira.example.com/rest/api/2/issue/10001",
        "unknown": { "ignored": true },
        "fields": {
            "summary": "Rate limit POST /v1/charges in payments-api",
            "description": "Calls to {{Ledger::charge}} time out.\n{code:java}retry();{code}",
            "status": { "name": "In Progress" },
            "resolution": null,
            "priority": { "name": "High" },
            "issuetype": { "name": "Bug" },
            "labels": ["perf", "q3"],
            "components": [{ "name": "Ledger" }],
            "fixVersions": [{ "name": "2.1" }],
            "assignee": { "displayName": "Alice Smith", "emailAddress": "Alice@Example.com", "name": "asmith" },
            "reporter": { "displayName": "Bob Jones", "accountId": "5b10ac" },
            "created": "2026-01-05T10:00:00.000+0000",
            "updated": "2026-01-06T10:00:00.000+0000",
            "duedate": "2026-02-01",
            "parent": { "key": "PAY-100" },
            "subtasks": [{ "key": "PAY-124" }],
            "issuelinks": [
                {
                    "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                    "inwardIssue": { "key": "PAY-7", "fields": { "summary": "Vendor contract unsigned" } }
                },
                {
                    "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                    "outwardIssue": { "key": "PAY-200", "fields": { "summary": "Launch" } }
                },
                {
                    "type": { "name": "Depends", "inward": "is depended on by", "outward": "depends on" },
                    "outwardIssue": { "key": "PLAT-1" }
                },
                {
                    "type": { "name": "Depends", "inward": "is depended on by", "outward": "depends on" },
                    "inwardIssue": { "key": "PLAT-2" }
                },
                {
                    "type": { "name": "Duplicate", "inward": "is duplicated by", "outward": "duplicates" },
                    "inwardIssue": { "key": "PAY-9" }
                },
                {
                    "type": { "name": "Supersession", "inward": "is superseded by", "outward": "supersedes" },
                    "inwardIssue": { "key": "PAY-300" }
                },
                { "type": { "name": "Polaris" }, "outwardIssue": { "key": "PAY-400" } },
                { "type": { "name": "Blocks" } }
            ],
            "comment": { "comments": [
                {
                    "id": "501",
                    "author": { "displayName": "Bob Jones", "accountId": "5b10ac" },
                    "body": "See PAY-7 and `src/ledger.rs`.",
                    "created": "2026-01-05T11:00:00.000+0000"
                },
                { "id": "502", "body": "   " }
            ] }
        }
    })
}

#[test]
fn ticket_node_carries_attributes_people_and_hierarchy() {
    let unit = parse(full_issue());
    assert_eq!(unit.kind, UnitKind::Ticket);
    assert_eq!(unit.key.as_str(), "jira/ticket/PAY-123");
    let ticket = keys::ticket("PAY-123");
    let node = unit.nodes.iter().find(|node| node.key == ticket).unwrap();
    assert_eq!(node.kind, NodeKind::Ticket);
    assert_eq!(
        node.name,
        "PAY-123: Rate limit POST /v1/charges in payments-api"
    );
    assert_eq!(node.aliases, vec!["PAY-123"]);
    assert_eq!(node.attrs[attrs::STATUS], "in progress");
    assert_eq!(node.attrs[attrs::PRIORITY], "High");
    assert_eq!(node.attrs[attrs::ISSUE_TYPE], "bug");
    assert_eq!(node.attrs[attrs::LABELS], json!(["perf", "q3"]));
    assert_eq!(node.attrs[attrs::CREATED_AT], json!(1_767_607_200_000_i64));
    assert_eq!(
        node.attrs[attrs::URL],
        "https://jira.example.com/browse/PAY-123"
    );
    assert!(!node.attrs.contains_key(attrs::RESOLUTION));
    assert!(!node.attrs.contains_key(attrs::BLOCKING));

    let me = NodeRef::Key(ticket);
    let alice = NodeRef::Key(keys::person("asmith"));
    edge(&unit, &me, EdgeKind::AssignedTo, &alice);
    let alice_node = unit
        .nodes
        .iter()
        .find(|n| n.key.as_str() == "person:asmith");
    assert_eq!(alice_node.unwrap().attrs[attrs::EMAIL], "alice@example.com");
    let bob = unit
        .nodes
        .iter()
        .find(|node| node.name == "Bob Jones")
        .unwrap();
    assert!(bob.aliases.contains(&"5b10ac".to_owned()));
    edge(
        &unit,
        &me,
        EdgeKind::AuthoredBy,
        &NodeRef::Key(bob.key.clone()),
    );

    edge(&unit, &key_ref("PAY-100"), EdgeKind::Contains, &me);
    edge(&unit, &me, EdgeKind::Contains, &key_ref("PAY-124"));
}

#[test]
fn links_are_directed_by_their_phrase() {
    let unit = parse(full_issue());
    let me = key_ref("PAY-123");
    // Inward form: PAY-7 blocks this issue; the reason is the blocker's summary.
    let inward = edge(&unit, &key_ref("PAY-7"), EdgeKind::Blocks, &me);
    assert_eq!(inward.attrs[attrs::REASON], "Vendor contract unsigned");
    assert_eq!(inward.attrs["link_type"], "Blocks");
    // Outward form: this issue blocks PAY-200.
    let outward = edge(&unit, &me, EdgeKind::Blocks, &key_ref("PAY-200"));
    assert_eq!(
        outward.attrs[attrs::REASON],
        "Rate limit POST /v1/charges in payments-api"
    );
    edge(&unit, &me, EdgeKind::DependsOn, &key_ref("PLAT-1"));
    edge(&unit, &key_ref("PLAT-2"), EdgeKind::DependsOn, &me);
    edge(&unit, &me, EdgeKind::RelatesTo, &key_ref("PAY-9"));
    edge(&unit, &key_ref("PAY-300"), EdgeKind::Supersedes, &me);
    edge(&unit, &me, EdgeKind::RelatesTo, &key_ref("PAY-400"));
    assert_eq!(
        unit.edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Blocks)
            .count(),
        2,
        "a link without an issue is ignored"
    );
}

#[test]
fn components_summary_and_text_become_tracks_and_mentions() {
    let unit = parse(full_issue());
    let me = key_ref("PAY-123");
    let component = NodeRef::Lookup(Lookup::new(
        &[NodeKind::Component, NodeKind::Service],
        "Ledger",
    ));
    assert_eq!(edge(&unit, &me, EdgeKind::Tracks, &component).weight, 1.0);
    let tracked: Vec<&str> = unit
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Tracks && edge.weight == 0.6)
        .filter_map(|edge| match &edge.to {
            NodeRef::Lookup(lookup) => Some(lookup.text.as_str()),
            NodeRef::Key(_) => None,
        })
        .collect();
    assert!(tracked.contains(&"POST /v1/charges"), "{tracked:?}");
    assert!(tracked.contains(&"payments-api"), "{tracked:?}");

    let description = &unit
        .chunks
        .iter()
        .find(|c| c.kind == ChunkKind::Description);
    assert_eq!(
        description.unwrap().text,
        "Calls to `Ledger::charge` time out.\n```retry();```"
    );
    assert!(unit.edges.iter().any(|edge| edge.from == me
        && edge.kind == EdgeKind::Mentions
        && matches!(&edge.to, NodeRef::Lookup(lookup) if lookup.text == "Ledger::charge")));
}

#[test]
fn comments_form_one_thread() {
    let unit = parse(full_issue());
    let thread = keys::thread(&source(), "PAY-123");
    let comment = keys::comment(&source(), "PAY-123-501");
    let thread_node = unit.nodes.iter().find(|node| node.key == thread).unwrap();
    assert_eq!(thread_node.attrs[attrs::THREAD_STATUS], attrs::THREAD_OPEN);
    assert_eq!(thread_node.attrs[attrs::THREAD_KIND], "page");
    let comments: Vec<_> = unit
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Comment)
        .collect();
    assert_eq!(comments.len(), 1, "blank comments are dropped");
    assert_eq!(comments[0].name, "Bob Jones on PAY-123");
    let thread_ref = NodeRef::Key(thread);
    let comment_ref = NodeRef::Key(comment.clone());
    edge(&unit, &key_ref("PAY-123"), EdgeKind::Contains, &thread_ref);
    edge(&unit, &thread_ref, EdgeKind::Contains, &comment_ref);
    assert!(
        unit.edges
            .iter()
            .any(|edge| edge.from == comment_ref && edge.kind == EdgeKind::AuthoredBy)
    );
    let chunk = unit.chunks.iter().find(|chunk| chunk.node == comment);
    assert_eq!(chunk.unwrap().kind, ChunkKind::Comment);
    let mentions = Lookup::new(&[NodeKind::Ticket], "PAY-7");
    edge(
        &unit,
        &comment_ref,
        EdgeKind::Mentions,
        &NodeRef::Lookup(mentions),
    );
}

#[test]
fn adf_description_is_converted_with_code_kept() {
    let unit = parse(json!({
        "key": "PAY-1",
        "fields": { "description": {
            "type": "doc", "version": 1,
            "content": [
                { "type": "heading", "content": [{ "type": "text", "text": "Problem" }] },
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Call " },
                    { "type": "text", "text": "Ledger::charge", "marks": [{ "type": "code" }] },
                    { "type": "hardBreak" },
                    { "type": "text", "text": "docs", "marks": [{ "type": "link", "attrs": { "href": "https://x.test/d" } }] },
                    { "type": "mention", "attrs": { "id": "1", "text": "@Alice" } }
                ] },
                { "type": "bulletList", "content": [
                    { "type": "listItem", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "one" }] }] },
                    { "type": "listItem", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "two" }] }] }
                ] },
                { "type": "codeBlock", "attrs": { "language": "rust" }, "content": [{ "type": "text", "text": "fn main() {}" }] },
                { "type": "mediaSingle", "content": [{ "type": "media", "attrs": { "id": "x" } }] },
                { "type": "futureNode", "content": [{ "type": "text", "text": "kept" }] }
            ]
        } }
    }));
    assert_eq!(
        unit.chunks[0].text,
        "Problem\n\nCall `Ledger::charge`\ndocs (https://x.test/d)@Alice\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\nkept"
    );
}

#[test]
fn blocked_state_comes_from_status_flag_or_custom_field() {
    let by_field = parse(json!({
        "key": "PAY-2",
        "names": { "customfield_10050": "Blocked Reason" },
        "fields": {
            "summary": "Ship it",
            "status": "Open",
            "customfield_10050": { "value": "Waiting for legal sign-off" },
            "issuelinks": [{
                "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                "inwardIssue": { "key": "LEG-4", "fields": { "summary": "Review terms" } }
            }]
        }
    }));
    assert_eq!(
        ticket_attr(&by_field, "PAY-2", attrs::BLOCKING),
        Some(&json!(true))
    );
    assert_eq!(
        ticket_attr(&by_field, "PAY-2", attrs::REASON),
        Some(&json!("Waiting for legal sign-off"))
    );
    // The custom reason on the blocked issue wins over the blocker's summary.
    let link = edge(
        &by_field,
        &key_ref("LEG-4"),
        EdgeKind::Blocks,
        &key_ref("PAY-2"),
    );
    assert_eq!(link.attrs[attrs::REASON], "Waiting for legal sign-off");

    let by_status = parse(json!({
        "key": "PAY-3",
        "fields": {
            "status": { "name": "Blocked" },
            "issuelinks": [{ "type": { "name": "Blocks" }, "inwardIssue": { "key": "PAY-7", "fields": { "summary": "Vendor" } } }]
        }
    }));
    assert_eq!(
        ticket_attr(&by_status, "PAY-3", attrs::BLOCKING),
        Some(&json!(true))
    );
    assert_eq!(
        ticket_attr(&by_status, "PAY-3", attrs::REASON),
        Some(&json!("blocked by PAY-7 (Vendor)"))
    );
    edge(
        &by_status,
        &key_ref("PAY-7"),
        EdgeKind::Blocks,
        &key_ref("PAY-3"),
    );

    let by_flag = parse(json!({
        "key": "PAY-4",
        "names": { "customfield_10021": "Flagged" },
        "fields": { "customfield_10021": [{ "value": "Impediment" }] }
    }));
    assert_eq!(
        ticket_attr(&by_flag, "PAY-4", attrs::BLOCKING),
        Some(&json!(true))
    );
    assert_eq!(ticket_attr(&by_flag, "PAY-4", attrs::REASON), None);

    let options = JiraOptions {
        blocked_fields: vec!["customfield_7".to_owned()],
        ..JiraOptions::default()
    };
    let issue = json!({ "key": "PAY-5", "fields": { "customfield_7": "no capacity" } });
    let configured = parse_issue(&source(), &issue, &options).unwrap();
    assert_eq!(
        ticket_attr(&configured, "PAY-5", attrs::REASON),
        Some(&json!("no capacity"))
    );
    assert_eq!(ticket_attr(&parse(issue), "PAY-5", attrs::BLOCKING), None);
}

#[test]
fn malformed_issues_are_errors_or_degrade_gracefully() {
    let options = JiraOptions::default();
    for bad in [
        json!(null),
        json!("PAY-1"),
        json!({}),
        json!({ "key": 7 }),
        json!({ "key": "has space" }),
    ] {
        assert!(parse_issue(&source(), &bad, &options).is_err(), "{bad}");
    }
    let odd = parse(json!({
        "key": "PAY-6",
        "fields": {
            "summary": 5, "status": [], "labels": "x", "components": {}, "assignee": 3,
            "parent": "PAY-1", "issuelinks": [1, { "inwardIssue": { "key": null } }],
            "comment": { "comments": "none" }, "created": "yesterday", "description": 12
        }
    }));
    assert_eq!(odd.nodes.len(), 1);
    assert_eq!(odd.nodes[0].name, "PAY-6");
    assert!(odd.edges.is_empty() && odd.chunks.is_empty());
    assert_eq!(parse(json!({ "key": "PAY-6", "fields": 1 })).nodes.len(), 1);
}

#[test]
fn scan_accepts_all_shapes_reports_bad_files_and_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, value: &str| std::fs::write(dir.path().join(name), value).unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    write("single.json", &full_issue().to_string());
    write(
        "search.json",
        &json!({
            "total": 2,
            "names": { "customfield_1": "Blocked Reason" },
            "issues": [
                { "key": "PAY-10", "fields": { "customfield_1": "waiting", "updated": "2026-01-01T00:00:00Z" } },
                { "fields": {} }
            ]
        })
        .to_string(),
    );
    write(
        "nested/array.json",
        &json!([
            { "key": "PAY-11" },
            { "key": "PAY-10", "fields": { "summary": "newer", "updated": "2026-03-01T00:00:00Z" } }
        ])
        .to_string(),
    );
    write("broken.json", "{ not json");
    write("scalar.json", "42");
    write("notes.txt", "ignored");

    let options = JiraOptions::default();
    let out = scan_jira(dir.path(), &source(), &options).unwrap();
    let keys: Vec<&str> = out.units.iter().map(|unit| unit.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "jira/ticket/PAY-10",
            "jira/ticket/PAY-11",
            "jira/ticket/PAY-123"
        ]
    );
    // The search-level `names` map applies, and the more recent duplicate wins.
    assert_eq!(
        ticket_attr(&out.units[0], "PAY-10", attrs::TITLE),
        Some(&json!("newer"))
    );
    let warned: Vec<&str> = out.warnings.iter().map(|w| w.location.as_str()).collect();
    assert_eq!(
        warned,
        [
            "broken.json",
            "nested/array.json",
            "scalar.json",
            "search.json#1"
        ]
    );

    let search = std::fs::read_to_string(dir.path().join("search.json")).unwrap();
    let parsed = parse_jira_document(&source(), "search.json", &search, &options);
    assert_eq!(
        ticket_attr(&parsed.units[0], "PAY-10", attrs::REASON),
        Some(&json!("waiting"))
    );

    let again = scan_jira(dir.path(), &source(), &options).unwrap();
    assert_eq!(out, again);
    for (first, second) in out.units.iter().zip(&again.units) {
        assert_eq!(
            first.content_hash().unwrap(),
            second.content_hash().unwrap()
        );
        let mut canonical = first.clone();
        canonical.canonicalize();
        assert_eq!(&canonical, first, "units are returned in canonical form");
    }
    assert!(scan_jira(&dir.path().join("missing"), &source(), &options).is_err());
}
