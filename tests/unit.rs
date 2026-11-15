//! Unit tests of the specification, through the public API only:
//!
//! * incremental Tree-sitter parsing accuracy across languages;
//! * trust-aware pathfinding: `SourceOfTruth` overrides `ReferenceOnly`;
//! * wiki comment / dismissed-thread parsing and the org-model hierarchy edges;
//! * Ollama auto-provisioning, teardown and rebuild with the in-process fakes.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anchor::base::{
    EdgeId, EdgeKind, GraphDelta, GraphEdge, GraphNode, NodeId, NodeKind, NodeRef, SourceId,
    TrustLevel, Unit, attrs, keys,
};
use anchor::config::provision::testing::{FakeOllama, FixedLocator, TestRig};
use anchor::config::provision::{
    BinarySource, OllamaManager, ProvisionError, RebuildOptions, ServerMode, TeardownOptions,
    no_progress,
};
use anchor::engine::graph::{
    ContextGraph, Goal, GraphConfig, Limits, Pathfinder, Seed, TraversalProfile,
};
use anchor::engine::ingest::org::parse_org_model;
use anchor::engine::ingest::wiki::{WikiOptions, parse_wiki_page};
use anchor::engine::parser::{IncrementalParser, Language, ParserConfig, parse_source};

// =============================================================================================
// Incremental parsing
// =============================================================================================

/// How to write a file of one language from a list of `(function name, constant)` pairs.
struct Dialect {
    language: Language,
    prefix: &'static str,
    suffix: &'static str,
    function: fn(&str, u32) -> String,
    /// Qualified-name prefix of the functions (`Calc::` for Java methods).
    qualifier: &'static str,
    /// Name of the enclosing symbol that changes whenever a member changes.
    container: Option<&'static str>,
}

type Edit = fn(&mut Vec<(&'static str, u32)>);
type Step = (
    &'static str,
    Edit,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

fn dialects() -> Vec<Dialect> {
    vec![
        Dialect {
            language: Language::Rust,
            prefix: "//! Mock module.\n\nuse std::fmt;\n\n",
            suffix: "",
            function: |n, k| {
                format!("/// Doc of {n}.\npub fn {n}(x: i64) -> i64 {{\n    x + {k}\n}}\n\n")
            },
            qualifier: "",
            container: None,
        },
        Dialect {
            language: Language::Python,
            prefix: "\"\"\"Mock module.\"\"\"\nimport os\n\n",
            suffix: "",
            function: |n, k| {
                format!("def {n}(x):\n    \"\"\"Doc of {n}.\"\"\"\n    return x + {k}\n\n\n")
            },
            qualifier: "",
            container: None,
        },
        Dialect {
            language: Language::JavaScript,
            prefix: "import fs from 'fs';\n\n",
            suffix: "",
            function: |n, k| {
                format!("/** Doc of {n}. */\nfunction {n}(x) {{\n  return x + {k};\n}}\n\n")
            },
            qualifier: "",
            container: None,
        },
        Dialect {
            language: Language::TypeScript,
            prefix: "import { x } from './x';\n\n",
            suffix: "",
            function: |n, k| {
                format!("export function {n}(x: number): number {{\n  return x + {k};\n}}\n\n")
            },
            qualifier: "",
            container: None,
        },
        Dialect {
            language: Language::Go,
            prefix: "package mock\n\nimport \"fmt\"\n\n",
            suffix: "",
            function: |n, k| {
                format!(
                    "// {n} adds.\nfunc {n}(x int) int {{\n\tfmt.Println(x)\n\treturn x + {k}\n}}\n\n"
                )
            },
            qualifier: "",
            container: None,
        },
        Dialect {
            language: Language::Java,
            prefix: "package mock;\n\npublic class Calc {\n",
            suffix: "}\n",
            function: |n, k| {
                format!("    public int {n}(int x) {{\n        return x + {k};\n    }}\n\n")
            },
            qualifier: "Calc::",
            container: Some("Calc"),
        },
        Dialect {
            language: Language::C,
            prefix: "#include <stdio.h>\n\n",
            suffix: "",
            function: |n, k| {
                format!("int {n}(int x) {{\n    printf(\"%d\", x);\n    return x + {k};\n}}\n\n")
            },
            qualifier: "",
            container: None,
        },
    ]
}

impl Dialect {
    fn render(&self, functions: &[(&str, u32)]) -> String {
        let mut text = self.prefix.to_owned();
        for (name, k) in functions {
            text.push_str(&(self.function)(name, *k));
        }
        text.push_str(self.suffix);
        text
    }

    fn names(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| format!("{}{n}", self.qualifier))
            .collect()
    }
}

fn without_container(dialect: &Dialect, names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| Some(n.as_str()) != dialect.container)
        .cloned()
        .collect()
}

#[test]
fn incremental_parsing_equals_from_scratch_and_reports_exact_symbol_deltas() {
    for dialect in dialects() {
        let parser = IncrementalParser::new(&ParserConfig::default());
        let key = format!("mock.{}", dialect.language.name());
        let mut current: Vec<(&str, u32)> = vec![("alpha", 1), ("beta", 2), ("gamma", 3)];

        // Each step: (description, edit, expected added, removed, modified).
        let steps: Vec<Step> = vec![
            ("modify a body", |f| f[1].1 = 20, &[], &[], &["beta"]),
            (
                "insert a function",
                |f| f.insert(2, ("delta", 4)),
                &["delta"],
                &[],
                &[],
            ),
            (
                "remove a function",
                |f| {
                    f.remove(0);
                },
                &[],
                &["alpha"],
                &[],
            ),
            (
                "rename a function",
                |f| f[2].0 = "gamma2",
                &["gamma2"],
                &["gamma"],
                &[],
            ),
            ("no change", |_| {}, &[], &[], &[]),
            ("reorder", |f| f.swap(0, 1), &[], &[], &[]),
        ];

        let text = dialect.render(&current);
        let first = parser.parse(&key, dialect.language, &text).unwrap();
        assert_eq!(first.parsed, parse_source(dialect.language, &text).unwrap());
        assert!(
            !first.reused_tree,
            "{}: first parse",
            dialect.language.name()
        );
        assert_eq!(
            without_container(&dialect, &first.delta.added),
            dialect.names(&["alpha", "beta", "gamma"]),
            "{}: first parse adds everything",
            dialect.language.name()
        );
        assert!(
            first.parsed.syntax_errors.is_empty(),
            "{:?}",
            first.parsed.syntax_errors
        );

        for (what, edit, added, removed, modified) in steps {
            edit(&mut current);
            let text = dialect.render(&current);
            let incremental = parser.parse(&key, dialect.language, &text).unwrap();
            let scratch = parse_source(dialect.language, &text).unwrap();
            let label = format!("{} / {what}", dialect.language.name());
            assert_eq!(
                incremental.parsed, scratch,
                "{label}: incremental != scratch"
            );
            assert!(incremental.reused_tree, "{label}: tree reused");
            let mut want_added = dialect.names(added);
            let mut want_modified = dialect.names(modified);
            if dialect.container.is_some() && what != "no change" {
                // The class symbol changes whenever one of its members does.
                want_modified.push("Calc".into());
            }
            want_modified.sort();
            let mut got_modified = incremental.delta.modified.clone();
            got_modified.sort();
            want_added.sort();
            let mut got_added = incremental.delta.added.clone();
            got_added.sort();
            assert_eq!(got_added, want_added, "{label}: added");
            assert_eq!(
                without_container(&dialect, &incremental.delta.removed),
                dialect.names(removed),
                "{label}: removed"
            );
            assert_eq!(got_modified, want_modified, "{label}: modified");
            if what == "no change" {
                assert!(incremental.delta.is_empty() && incremental.changed_ranges.is_empty());
            }
        }

        // A multi-byte edit in front shifts every byte offset (and every symbol hash stays).
        let before = dialect.render(&current);
        let shifted = match dialect.language {
            Language::Python => format!("# héllo wörld ✓ 日本語\n{before}"),
            _ => format!("// héllo wörld ✓ 日本語\n{before}"),
        };
        let incremental = parser.parse(&key, dialect.language, &shifted).unwrap();
        assert_eq!(
            incremental.parsed,
            parse_source(dialect.language, &shifted).unwrap(),
            "{}: multi-byte insertion",
            dialect.language.name()
        );
        assert!(
            incremental.delta.added.is_empty() && incremental.delta.removed.is_empty(),
            "{}: {:?}",
            dialect.language.name(),
            incremental.delta
        );

        // Breaking the syntax is reported identically by both paths, and repairing it recovers.
        let broken = format!("{shifted}\n)))) {{ ((\n");
        let incremental = parser.parse(&key, dialect.language, &broken).unwrap();
        assert_eq!(
            incremental.parsed,
            parse_source(dialect.language, &broken).unwrap(),
            "{}: broken syntax",
            dialect.language.name()
        );
        assert!(!incremental.parsed.syntax_errors.is_empty());
        let repaired = parser.parse(&key, dialect.language, &shifted).unwrap();
        assert_eq!(
            repaired.parsed,
            parse_source(dialect.language, &shifted).unwrap()
        );
        assert!(repaired.parsed.syntax_errors.is_empty());
    }
}

#[test]
fn a_long_random_edit_sequence_never_diverges_from_a_fresh_parse() {
    // Deterministic pseudo-random edits of a Rust file: insert, delete and rewrite lines.
    let parser = IncrementalParser::new(&ParserConfig::default());
    let mut lines: Vec<String> = (0..30)
        .map(|i| format!("pub fn f{i}(x: u32) -> u32 {{ x + {i} }}"))
        .collect();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for step in 0..200 {
        let at = (next() % lines.len() as u64) as usize;
        match next() % 3 {
            0 => lines.insert(at, format!("pub fn g{step}(y: u8) -> u8 {{ y ^ {step} }}")),
            1 if lines.len() > 5 => {
                lines.remove(at);
            }
            _ => lines[at] = format!("pub struct S{step} {{ field: u{} }}", 8 << (step % 4)),
        }
        let text = lines.join("\n");
        let incremental = parser.parse("random.rs", Language::Rust, &text).unwrap();
        assert_eq!(
            incremental.parsed,
            parse_source(Language::Rust, &text).unwrap(),
            "diverged at step {step}"
        );
    }
}

// =============================================================================================
// Pathfinding: SourceOfTruth overrides ReferenceOnly
// =============================================================================================

const SOT: TrustLevel = TrustLevel::SourceOfTruth;
const REF: TrustLevel = TrustLevel::ReferenceOnly;

fn node(id: i64, kind: NodeKind, trust: TrustLevel) -> GraphNode {
    GraphNode {
        id: NodeId(id),
        kind,
        trust,
    }
}

fn edge(id: i64, from: i64, to: i64, kind: EdgeKind, trust: TrustLevel) -> GraphEdge {
    GraphEdge {
        id: EdgeId(id),
        from: NodeId(from),
        to: NodeId(to),
        kind,
        trust,
        weight: 1.0,
    }
}

/// An API (1) implemented by service (2), contained in `depth` nested containers (3..), the
/// outermost owned by authoritative team 100; team 101 claims to own the API directly with the
/// given trust.
fn ownership_graph(depth: i64, claim: TrustLevel, claimant: TrustLevel) -> ContextGraph {
    let mut nodes = vec![
        node(1, NodeKind::ApiSpec, SOT),
        node(2, NodeKind::Service, SOT),
        node(100, NodeKind::Team, SOT),
        node(101, NodeKind::Team, claimant),
    ];
    let mut edges = vec![edge(1, 2, 1, EdgeKind::Implements, SOT)];
    let mut inner = 2;
    for level in 0..depth {
        let id = 3 + level;
        nodes.push(node(id, NodeKind::Component, SOT));
        edges.push(edge(10 + level, id, inner, EdgeKind::Contains, SOT));
        inner = id;
    }
    edges.push(edge(50, 100, inner, EdgeKind::Owns, SOT));
    edges.push(edge(51, 101, 1, EdgeKind::Owns, claim));
    ContextGraph::from_parts(nodes, edges)
}

fn teams(graph: &ContextGraph, config: &GraphConfig) -> Vec<anchor::engine::graph::Path> {
    Pathfinder::new(graph, config).nearest(
        &[Seed::new(NodeId(1))],
        &Goal::kinds(&[NodeKind::Team]),
        5,
        &TraversalProfile::ownership(),
        Limits::from(config),
    )
}

#[test]
fn source_of_truth_overrides_reference_only_at_every_distance() {
    let config = GraphConfig::default();
    for depth in 0..=3 {
        let graph = ownership_graph(depth, REF, REF);
        let paths = teams(&graph, &config);
        assert_eq!(paths.len(), 2, "depth {depth}");
        let resolved = Pathfinder::new(&graph, &config)
            .resolve_conflict(paths)
            .unwrap();
        assert_eq!(resolved.winner.target(), Some(NodeId(100)), "depth {depth}");
        assert!(resolved.authoritative);
        assert_eq!(resolved.overridden.len(), 1);
        assert_eq!(resolved.overridden[0].target(), Some(NodeId(101)));
        assert!(!resolved.overridden[0].cost.is_authoritative());
    }
}

#[test]
fn direct_authoritative_claim_beats_direct_reference_claim_in_any_candidate_order() {
    // Two teams own the same API directly, one per trust level, at the same distance.
    let graph = ContextGraph::from_parts(
        [
            node(1, NodeKind::ApiSpec, SOT),
            node(10, NodeKind::Team, SOT),
            node(11, NodeKind::Team, SOT),
        ],
        [
            edge(1, 10, 1, EdgeKind::Owns, SOT),
            edge(2, 11, 1, EdgeKind::Owns, REF),
        ],
    );
    let config = GraphConfig::default();
    let finder = Pathfinder::new(&graph, &config);
    let paths = teams(&graph, &config);
    assert_eq!(paths.len(), 2);
    let mut reversed = paths.clone();
    reversed.reverse();
    for candidates in [paths, reversed] {
        let resolved = finder.resolve_conflict(candidates).unwrap();
        assert_eq!(resolved.winner.target(), Some(NodeId(10)));
        assert!(resolved.authoritative);
    }
}

#[test]
fn reference_only_node_with_an_authoritative_edge_still_counts_as_reference() {
    // The claimant team is only known to a wiki (ReferenceOnly node), the edge is authoritative:
    // the path still uses one reference element, so the fully authoritative owner wins.
    let graph = ownership_graph(1, SOT, REF);
    let config = GraphConfig::default();
    let paths = teams(&graph, &config);
    let resolved = Pathfinder::new(&graph, &config)
        .resolve_conflict(paths)
        .unwrap();
    assert_eq!(resolved.winner.target(), Some(NodeId(100)));
}

#[test]
fn reference_only_claim_is_the_fallback_when_no_authoritative_owner_exists() {
    let mut graph = ownership_graph(2, REF, REF);
    graph.apply(&GraphDelta {
        edges_removed: vec![EdgeId(50)],
        ..GraphDelta::default()
    });
    let config = GraphConfig::default();
    let paths = teams(&graph, &config);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].target(), Some(NodeId(101)));
    let resolved = Pathfinder::new(&graph, &config)
        .resolve_conflict(paths)
        .unwrap();
    assert!(
        !resolved.authoritative,
        "fallback is flagged as non-authoritative"
    );
    assert!(resolved.overridden.is_empty());
}

#[test]
fn non_strict_mode_lets_a_much_nearer_reference_claim_win() {
    let graph = ownership_graph(3, REF, REF);
    let mut config = GraphConfig::default();
    config.trust.strict_authority = false;
    config.trust.reference_only_multiplier = 1.5;
    let paths = teams(&graph, &config);
    assert_eq!(paths[0].target(), Some(NodeId(101)), "weighted cost only");
    // The same graph in strict mode picks the authoritative owner.
    let strict = teams(&graph, &GraphConfig::default());
    assert_eq!(strict[0].target(), Some(NodeId(100)));
    // And with a heavy multiplier even non-strict mode prefers the authoritative path.
    config.trust.reference_only_multiplier = 50.0;
    config.max_cost = 1000.0;
    let heavy = teams(&graph, &config);
    assert_eq!(heavy[0].target(), Some(NodeId(100)));
}

#[test]
fn results_do_not_depend_on_insertion_order() {
    let forward = ownership_graph(2, REF, REF);
    let nodes: Vec<GraphNode> = forward.nodes().collect();
    let edges: Vec<GraphEdge> = forward.edges().collect();
    let backward = ContextGraph::from_parts(nodes.into_iter().rev(), edges.into_iter().rev());
    let config = GraphConfig::default();
    let targets = |graph: &ContextGraph| -> Vec<_> {
        teams(graph, &config).iter().map(|p| p.target()).collect()
    };
    assert_eq!(targets(&forward), targets(&backward));
}

// =============================================================================================
// Wiki comment threads and dismissed discussions
// =============================================================================================

fn wiki_unit(rel_path: &str, page: &str, sidecar: Option<&str>) -> Unit {
    let mut output = parse_wiki_page(
        &SourceId::new("wiki"),
        rel_path,
        page.as_bytes(),
        sidecar.map(str::as_bytes),
        &WikiOptions::default(),
    )
    .unwrap();
    assert!(output.warnings.is_empty(), "{:?}", output.warnings);
    assert_eq!(output.units.len(), 1);
    output.units.remove(0)
}

fn attr<'a>(unit: &'a Unit, key: &anchor::base::NodeKey, name: &str) -> Option<&'a str> {
    unit.nodes
        .iter()
        .find(|n| &n.key == key)?
        .attrs
        .get(name)?
        .as_str()
}

fn has_edge(
    unit: &Unit,
    from: &anchor::base::NodeKey,
    kind: EdgeKind,
    to: &anchor::base::NodeKey,
) -> bool {
    unit.edges.iter().any(|e| {
        e.kind == kind && e.from == NodeRef::Key(from.clone()) && e.to == NodeRef::Key(to.clone())
    })
}

const HTML_PAGE: &str = r#"<html><head><title>Payments Runbook</title>
<meta name="page-id" content="runbook"></head><body><main><h1>Payments Runbook</h1>
<p>Retries are handled by the payments-api service.</p></main>
<div class="comment-thread" data-thread-id="t-1" data-kind="review" data-status="dismissed"
     data-anchor="retry policy" data-closed-by="Bob Lee" data-close-reason="Superseded by PAY-123 idempotency keys">
  <div class="comment" data-comment-id="c-1" data-author="Alice Smith" data-created="2024-05-01T09:00:00Z">
    <div class="comment-body"><p>Should we retry forever?</p></div>
    <div class="comment" data-comment-id="c-2" data-author="Bob Lee">No, see PAY-123.</div>
  </div>
</div>
<div class="comment-thread" data-thread-id="t-2" data-status="open">
  <div class="comment" data-comment-id="c-1" data-author="Carol White">Still open.</div>
</div>
</body></html>"#;

#[test]
fn embedded_html_threads_keep_status_closer_reason_and_replies() {
    let unit = wiki_unit("ops/runbook.html", HTML_PAGE, None);
    let source = SourceId::new("wiki");
    let page = keys::wiki_page(&source, "runbook");
    let dismissed = keys::thread(&source, "runbook/t-1");
    let open = keys::thread(&source, "runbook/t-2");
    let first = keys::comment(&source, "runbook/t-1/c-1");
    let reply = keys::comment(&source, "runbook/t-1/c-2");

    assert_eq!(
        attr(&unit, &dismissed, attrs::THREAD_STATUS),
        Some(attrs::THREAD_DISMISSED)
    );
    assert_eq!(attr(&unit, &dismissed, attrs::THREAD_KIND), Some("review"));
    assert_eq!(attr(&unit, &dismissed, attrs::CLOSED_BY), Some("Bob Lee"));
    assert_eq!(
        attr(&unit, &dismissed, attrs::CLOSE_REASON),
        Some("Superseded by PAY-123 idempotency keys")
    );
    assert_eq!(
        attr(&unit, &dismissed, attrs::ANCHOR_TEXT),
        Some("retry policy")
    );
    assert_eq!(
        attr(&unit, &open, attrs::THREAD_STATUS),
        Some(attrs::THREAD_OPEN)
    );
    assert_eq!(attr(&unit, &open, attrs::CLOSED_BY), None);

    assert!(has_edge(&unit, &page, EdgeKind::Contains, &dismissed));
    assert!(has_edge(&unit, &dismissed, EdgeKind::Contains, &first));
    assert!(has_edge(&unit, &dismissed, EdgeKind::Contains, &reply));
    assert!(
        has_edge(&unit, &reply, EdgeKind::ReplyTo, &first),
        "nested comment replies"
    );
    assert!(!has_edge(&unit, &first, EdgeKind::ReplyTo, &reply));
    assert!(
        unit.edges
            .iter()
            .any(|e| e.kind == EdgeKind::DecidedBy && e.from == NodeRef::Key(dismissed.clone())),
        "the closer is linked to the thread"
    );
    // The reason is searchable text: a Decision chunk names the closer and the reason.
    let decision = unit
        .chunks
        .iter()
        .find(|c| c.text.starts_with("Dismissed by Bob Lee"))
        .expect("decision chunk");
    assert!(
        decision
            .text
            .contains("Superseded by PAY-123 idempotency keys")
    );
    // Comment bodies are kept, thread containers are not part of the page text.
    assert!(
        unit.chunks
            .iter()
            .any(|c| c.text.contains("Should we retry forever?"))
    );
    assert!(
        !unit
            .chunks
            .iter()
            .filter(|c| c.text.contains("Retries are handled"))
            .any(|c| c.text.contains("Still open"))
    );
}

#[test]
fn sidecar_threads_support_status_synonyms_and_reply_to() {
    let sidecar = r#"{"threads": [
      {"id": "a", "kind": "inline", "status": "rejected", "anchor": "exactly once",
       "closed_by": "Dana Fox", "close_reason": "Out of scope for Q3",
       "comments": [
         {"id": "1", "author": "Eli Park", "body": "Can we guarantee exactly once?"},
         {"id": "2", "author": {"name": "Dana Fox", "handle": "dfox"}, "body": "No.", "reply_to": "1"}]},
      {"id": "b", "status": "done", "comments": [{"id": "1", "author": "Eli Park", "body": "Typo fixed"}]},
      {"id": "c", "comments": [{"id": "1", "author": "Eli Park", "body": "Question"}]}
    ]}"#;
    let page = "---\nid: design\ntitle: Ledger Design\n---\n# Ledger Design\n\nWe write postings exactly once.\n";
    let unit = wiki_unit("design.md", page, Some(sidecar));
    let source = SourceId::new("wiki");
    let a = keys::thread(&source, "design/a");
    let b = keys::thread(&source, "design/b");
    let c = keys::thread(&source, "design/c");
    assert_eq!(
        attr(&unit, &a, attrs::THREAD_STATUS),
        Some("dismissed"),
        "rejected -> dismissed"
    );
    assert_eq!(
        attr(&unit, &b, attrs::THREAD_STATUS),
        Some("resolved"),
        "done -> resolved"
    );
    assert_eq!(attr(&unit, &c, attrs::THREAD_STATUS), Some("open"));
    assert_eq!(attr(&unit, &a, attrs::THREAD_KIND), Some("inline"));
    assert_eq!(
        attr(&unit, &c, attrs::THREAD_KIND),
        Some("page"),
        "no anchor -> page thread"
    );
    assert_eq!(attr(&unit, &a, attrs::CLOSED_BY), Some("Dana Fox"));
    assert_eq!(
        attr(&unit, &a, attrs::CLOSE_REASON),
        Some("Out of scope for Q3")
    );
    assert!(has_edge(
        &unit,
        &keys::comment(&source, "design/a/2"),
        EdgeKind::ReplyTo,
        &keys::comment(&source, "design/a/1")
    ));
    // Comment ids are scoped by thread: the same id "1" exists in three threads.
    for thread in ["a", "b", "c"] {
        assert!(
            unit.nodes
                .iter()
                .any(|n| n.key == keys::comment(&source, &format!("design/{thread}/1")))
        );
    }
}

#[test]
fn malformed_sidecars_and_unknown_statuses_warn_without_losing_the_page() {
    let output = parse_wiki_page(
        &SourceId::new("wiki"),
        "p.md",
        b"# P\n\ntext\n",
        Some(br#"{"threads": [{"id": "x", "status": "sideways", "comments": [{"id": "1", "author": "Z", "body": "hi"}]}]}"#),
        &WikiOptions::default(),
    )
    .unwrap();
    assert_eq!(output.units.len(), 1);
    assert!(!output.warnings.is_empty(), "unknown status is reported");
    let thread = keys::thread(&SourceId::new("wiki"), "p/x");
    assert_eq!(
        attr(&output.units[0], &thread, attrs::THREAD_STATUS),
        Some("open")
    );

    let broken = parse_wiki_page(
        &SourceId::new("wiki"),
        "p.md",
        b"# P\n",
        Some(b"{ not json"),
        &WikiOptions::default(),
    )
    .unwrap();
    assert_eq!(broken.units.len(), 1);
    assert!(!broken.warnings.is_empty());
    assert!(
        parse_wiki_page(
            &SourceId::new("wiki"),
            "p.png",
            b"x",
            None,
            &WikiOptions::default()
        )
        .is_err()
    );
}

// =============================================================================================
// Domain-portfolio hierarchy edges from the org model
// =============================================================================================

const ORG_YAML: &str = r#"
organisation: acme
portfolios:
  - name: Consumer Payments
    lead: asmith
    owner: payments-platform
    domains:
      - name: Billing
        lead: Bob Jones
        owner: "@acme/billing-core"
        components:
          - name: Ledger
            owner: payments-platform
            services:
              - payments-api
              - name: ledger-writer
                owner: bjones
      - name: Risk
        components:
          - name: Scoring
            services: [score-engine]
teams:
  - name: Payments Platform
    handle: payments-platform
    lead: asmith
    members:
      - asmith
      - { name: Carol White, handle: cwhite, role: SRE }
  - name: Billing Core
    handle: billing-core
    parent: Payments Platform
people:
  - name: Alice Smith
    handle: asmith
    team: payments-platform
  - name: Bob Jones
    handle: bjones
    reports_to: asmith
    team: billing-core
"#;

#[test]
fn org_model_maps_the_hierarchy_ownership_leadership_and_membership() {
    let output = parse_org_model(&SourceId::new("org"), "org.yaml", ORG_YAML).unwrap();
    assert!(output.warnings.is_empty(), "{:?}", output.warnings);
    let unit = &output.units[0];

    let portfolio = keys::portfolio("Consumer Payments");
    let (billing, risk) = (keys::domain("Billing"), keys::domain("Risk"));
    let (ledger, scoring) = (keys::component("Ledger"), keys::component("Scoring"));
    let (api, writer, score) = (
        keys::service("payments-api"),
        keys::service("ledger-writer"),
        keys::service("score-engine"),
    );
    // Portfolio -> Domain -> Component -> Service containment, all the way down.
    for (parent, child) in [
        (&portfolio, &billing),
        (&portfolio, &risk),
        (&billing, &ledger),
        (&risk, &scoring),
        (&ledger, &api),
        (&ledger, &writer),
        (&scoring, &score),
    ] {
        assert!(
            has_edge(unit, parent, EdgeKind::Contains, child),
            "{parent} contains {child}"
        );
    }
    assert!(
        !has_edge(unit, &portfolio, EdgeKind::Contains, &ledger),
        "no skipping levels"
    );
    assert!(!has_edge(unit, &billing, EdgeKind::Contains, &scoring));

    // Owners resolve to teams (preferred over people), `@org/team` to the handle, a person owner
    // to the person.
    let platform = keys::team("payments-platform");
    assert!(has_edge(unit, &platform, EdgeKind::Owns, &portfolio));
    assert!(has_edge(
        unit,
        &keys::team("billing-core"),
        EdgeKind::Owns,
        &billing
    ));
    assert!(has_edge(unit, &platform, EdgeKind::Owns, &ledger));
    assert!(has_edge(
        unit,
        &keys::person("bjones"),
        EdgeKind::Owns,
        &writer
    ));

    // Leaders and members.
    assert!(has_edge(
        unit,
        &keys::person("asmith"),
        EdgeKind::Leads,
        &portfolio
    ));
    assert!(has_edge(
        unit,
        &keys::person("bjones"),
        EdgeKind::Leads,
        &billing
    ));
    assert!(has_edge(
        unit,
        &keys::person("asmith"),
        EdgeKind::Leads,
        &platform
    ));
    assert!(has_edge(
        unit,
        &keys::person("asmith"),
        EdgeKind::MemberOf,
        &platform
    ));
    assert!(has_edge(
        unit,
        &keys::person("cwhite"),
        EdgeKind::MemberOf,
        &platform
    ));
    assert!(has_edge(
        unit,
        &keys::person("bjones"),
        EdgeKind::MemberOf,
        &keys::team("billing-core")
    ));
    // Parent team contains the sub team; reporting lines are kept.
    assert!(has_edge(
        unit,
        &platform,
        EdgeKind::Contains,
        &keys::team("billing-core")
    ));
    assert!(has_edge(
        unit,
        &keys::person("bjones"),
        EdgeKind::ReportsTo,
        &keys::person("asmith")
    ));

    // Hierarchy kinds on the nodes themselves.
    for (key, kind) in [
        (&portfolio, NodeKind::Portfolio),
        (&billing, NodeKind::Domain),
        (&ledger, NodeKind::Component),
        (&api, NodeKind::Service),
    ] {
        assert!(unit.nodes.iter().any(|n| &n.key == key && n.kind == kind));
    }
}

#[test]
fn org_model_flat_form_builds_the_same_hierarchy_and_reports_dangling_references() {
    let flat = r#"
portfolios: [ { name: Retail } ]
domains: [ { name: Checkout, portfolio: Retail } ]
components: [ { name: Cart, domain: Checkout } ]
services: [ { name: cart-api, component: Cart, owner: ghost-team } ]
"#;
    let output = parse_org_model(&SourceId::new("org"), "flat.yaml", flat).unwrap();
    let unit = &output.units[0];
    for (parent, child) in [
        (keys::portfolio("Retail"), keys::domain("Checkout")),
        (keys::domain("Checkout"), keys::component("Cart")),
        (keys::component("Cart"), keys::service("cart-api")),
    ] {
        assert!(has_edge(unit, &parent, EdgeKind::Contains, &child));
    }
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.message.contains("ghost-team")),
        "undefined owner is reported: {:?}",
        output.warnings
    );
}

// =============================================================================================
// Ollama auto-provisioning, teardown and rebuild
// =============================================================================================

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn manager_for(rig: &TestRig, base_url: String) -> OllamaManager {
    let mut config = rig.config.clone();
    config.base_url = base_url;
    OllamaManager::with_parts(
        config,
        rig.root().to_path_buf(),
        rig.launcher.clone(),
        Arc::new(FixedLocator(None)),
    )
    .with_home(None)
}

#[tokio::test]
async fn fresh_machine_is_provisioned_end_to_end() -> TestResult {
    let rig = TestRig::new().await?;
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(report.mode, ServerMode::ManagedStarted);
    assert!(report.installed && report.downloaded && report.started && report.pulled);
    assert_eq!(report.binary_source, Some(BinarySource::Managed));
    assert_eq!(report.model, "tiny:latest");
    let verification = report.verification.expect("verified with a generation");
    assert!(verification.completion_tokens > 0);
    assert!(rig.manager.layout().binary().is_file());
    assert!(rig.manager.layout().is_marked());
    assert_eq!(rig.launcher.running(), 1);
    // Everything lives under the managed root, nothing in $HOME or next to it.
    let spec = &rig.launcher.specs()[0];
    assert!(spec.models_dir.starts_with(rig.root()));

    // A second call changes nothing.
    let hits = rig.downloads.hits();
    let again = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(again.mode, ServerMode::ManagedRunning);
    assert!(!again.installed && !again.started && !again.pulled);
    assert_eq!(
        (rig.launcher.specs().len(), rig.downloads.hits()),
        (1, hits)
    );
    Ok(())
}

#[tokio::test]
async fn an_external_ollama_is_used_and_never_touched() -> TestResult {
    let rig = TestRig::new().await?;
    let external = FakeOllama::start_with(0, &["precious:1b"], None).await?;
    let manager = manager_for(&rig, external.base_url());
    let report = manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(report.mode, ServerMode::External);
    assert_eq!(report.base_url, external.base_url());
    assert!(report.pulled && !report.installed && !report.started);
    assert!(rig.launcher.specs().is_empty(), "no process launched");
    assert_eq!(rig.downloads.hits(), 0, "nothing downloaded");

    // Teardown and rebuild leave the server and its other models alone.
    let teardown = manager.teardown(&TeardownOptions::default()).await?;
    assert!(!teardown.stopped_server);
    assert!(external.state().deletes.lock().unwrap().is_empty());
    assert!(
        external
            .state()
            .models
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "precious:1b")
    );
    Ok(())
}

#[tokio::test]
async fn teardown_removes_exactly_what_anchor_installed() -> TestResult {
    let rig = TestRig::new().await?;
    // Neighbours that must survive: the database next to the managed root, the user's own Ollama
    // key pair, and an unknown file inside the root.
    let database = rig.dir.path().join("data").join("anchor.db");
    std::fs::create_dir_all(database.parent().unwrap())?;
    std::fs::write(&database, "database")?;
    let user_ollama = rig.dir.path().join("home").join(".ollama");
    std::fs::create_dir_all(&user_ollama)?;
    std::fs::write(user_ollama.join("id_ed25519"), "key")?;

    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    std::fs::write(rig.root().join("notes.txt"), "mine")?;
    let layout = rig.manager.layout();
    assert!(layout.dist_dir().is_dir() && layout.models_dir().is_dir());

    let teardown = rig.manager.teardown(&TeardownOptions::default()).await?;
    assert!(teardown.stopped_server);
    assert!(teardown.bytes_freed > 2048, "{}", teardown.bytes_freed);
    assert!(
        teardown
            .removed
            .iter()
            .all(|r| r.path.starts_with(rig.root())),
        "only paths inside the managed root: {:?}",
        teardown.removed
    );
    assert_eq!(rig.launcher.running(), 0);
    assert!(!layout.dist_dir().exists() && !layout.models_dir().exists());
    assert!(!layout.cache_dir().exists());
    assert_eq!(std::fs::read_to_string(&database)?, "database");
    assert!(user_ollama.join("id_ed25519").exists());
    assert!(
        rig.root().join("notes.txt").exists(),
        "unknown files are never deleted"
    );
    Ok(())
}

#[tokio::test]
async fn teardown_refuses_directories_it_does_not_own() -> TestResult {
    let rig = TestRig::new().await?;
    let precious = rig.dir.path().join("precious");
    std::fs::create_dir_all(&precious)?;
    std::fs::write(precious.join("file"), "keep")?;
    let manager = OllamaManager::with_parts(
        rig.config.clone(),
        &precious,
        rig.launcher.clone(),
        Arc::new(FixedLocator(None)),
    )
    .with_home(None);
    let error = manager
        .teardown(&TeardownOptions::default())
        .await
        .expect_err("unmarked directory");
    assert!(
        matches!(error, ProvisionError::UnsafePath { .. }),
        "{error}"
    );
    let error = manager
        .ensure_ready(&rig.options, &no_progress)
        .await
        .expect_err("will not adopt a foreign directory");
    assert!(
        matches!(error, ProvisionError::UnsafePath { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(precious.join("file"))?, "keep");
    Ok(())
}

#[tokio::test]
async fn rebuild_wipes_and_provisions_again() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let marker = rig.manager.layout().dist_dir().join("stale-file");
    std::fs::write(&marker, "left over from the old install")?;

    let report = rig
        .manager
        .rebuild(
            &RebuildOptions {
                provision: rig.options.clone(),
                ..RebuildOptions::default()
            },
            &no_progress,
        )
        .await?;
    assert!(report.teardown.stopped_server);
    assert!(report.provision.downloaded && report.provision.installed);
    assert!(report.provision.started && report.provision.pulled);
    assert!(report.verification.completion_tokens > 0);
    assert!(!marker.exists(), "the old distribution was wiped");
    assert!(rig.manager.layout().binary().is_file());
    assert_eq!(rig.launcher.running(), 1, "one server, the new one");
    assert!(report.total_ms >= report.teardown_ms.max(report.provision_ms));

    // Keeping the download cache: reinstalled without fetching the archive again.
    let report = rig
        .manager
        .rebuild(
            &RebuildOptions {
                teardown: TeardownOptions {
                    keep_cache: true,
                    ..TeardownOptions::default()
                },
                provision: rig.options.clone(),
            },
            &no_progress,
        )
        .await?;
    assert!(report.provision.installed && !report.provision.downloaded);
    Ok(())
}

#[tokio::test]
async fn a_crashed_managed_server_is_restarted_without_reinstalling() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    rig.launcher.crash_all();
    let status = rig.manager.status().await;
    assert!(!status.server_alive && status.distribution_present);
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert!(report.started && !report.installed);
    rig.manager.stop(Duration::from_secs(1)).await?;
    assert_eq!(rig.launcher.running(), 0);
    let _: &Path = rig.root();
    Ok(())
}
