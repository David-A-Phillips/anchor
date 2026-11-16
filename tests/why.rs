//! `anchor why` end to end on a real git repository: a line is traced to its commit, the pull
//! request named in the message, the dismissed review on that pull request, the ticket and the
//! wiki page that discusses the ticket.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anchor::base::{EdgeKind, NodeKind, SourceId, SourceKind, TrustLevel, keys};
use anchor::config::{AnchorConfig, Depth, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::router::ProviderKind;
use anchor::engine::why::render;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Alice Smith",
            "-c",
            "user.email=alice@example.com",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_AUTHOR_DATE", "2025-03-01T10:00:00Z")
        .env("GIT_COMMITTER_DATE", "2025-03-01T10:00:00Z")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A repository with three commits, plus review, tracker and wiki exports that refer to them.
fn world(dir: &Path, depth: Depth) -> Arc<Engine> {
    let repo = dir.join("ledger");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("CODEOWNERS"), "* @acme/payments-platform\n");
    write(
        &repo.join("src/charge.rs"),
        "/// Charges a card.\npub fn charge(amount: u64) -> u64 {\n    amount\n}\n",
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "Initial ledger"]);
    write(
        &repo.join("src/charge.rs"),
        "/// Charges a card.\npub fn charge(amount: u64) -> u64 {\n    retry(amount, 3)\n}\n\nfn retry(amount: u64, attempts: u32) -> u64 {\n    amount + u64::from(attempts)\n}\n",
    );
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "PAY-12: retry charges with a bounded backoff (#7)\n\nThe gateway drops one request in a thousand under load.",
        ],
    );
    write(&repo.join("README.md"), "# ledger\n\nThe ledger service.\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "Add a README"]);

    write(
        &dir.join("reviews/ledger-7.json"),
        &serde_json::json!({
            "pull_request": {
                "number": 7, "title": "Retry charges", "state": "closed", "merged": true,
                "body": "Implements PAY-12.",
                "html_url": "https://git.example/acme/ledger/pull/7",
                "user": { "login": "asmith" },
                "base": { "ref": "main", "repo": { "name": "ledger", "full_name": "acme/ledger" } },
                "head": { "ref": "feature/PAY-12" }
            },
            "reviews": [{
                "id": 71, "state": "DISMISSED", "dismissed_state": "CHANGES_REQUESTED",
                "body": "Unbounded retries will hammer the gateway.",
                "dismissal_message": "Dismissed: the retry count is capped at three in the follow-up commit.",
                "dismissed_by": { "login": "carol" },
                "user": { "login": "bob" }, "submitted_at": "2025-02-27T09:00:00Z"
            }],
            "review_comments": [], "review_threads": [], "issue_comments": []
        })
        .to_string(),
    );
    write(
        &dir.join("jira/PAY-12.json"),
        &serde_json::json!({
            "key": "PAY-12",
            "fields": {
                "summary": "Charges fail intermittently under load",
                "description": "Roughly 0.1% of charge requests are dropped by the gateway.",
                "status": { "name": "Done" }
            }
        })
        .to_string(),
    );
    write(
        &dir.join("wiki/gateway-incident.md"),
        "---\ntitle: Gateway incident review\n---\n# Gateway incident review\n\nPAY-12 tracks the dropped charge requests. We decided to retry at most three times.\n",
    );

    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    let mut ledger = SourceConfig::new("ledger", SourceKind::Repo, repo.to_string_lossy());
    ledger.depth = depth;
    config.sources.push(ledger);
    for (id, kind, trust) in [
        ("reviews", SourceKind::CodeReview, TrustLevel::ReferenceOnly),
        ("jira", SourceKind::Jira, TrustLevel::SourceOfTruth),
        ("wiki", SourceKind::Wiki, TrustLevel::ReferenceOnly),
    ] {
        let mut source = SourceConfig::new(id, kind, dir.join(id).to_string_lossy());
        source.trust = trust;
        config.sources.push(source);
    }
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    engine
}

#[test]
fn a_line_is_traced_to_its_commit_review_ticket_and_page() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path(), Depth::Full);

    // History is in the graph: three commits, linked to the file, the ticket and the pull request.
    let stats = engine.store().stats().unwrap();
    assert_eq!(stats.nodes["commit"], 3, "{stats:?}");
    let source = SourceId::new("ledger");
    let file = engine
        .store()
        .node_by_key(&keys::file(&source, "src/charge.rs"))
        .unwrap()
        .unwrap();
    let edges = engine.store().edges_of(file.id, 100).unwrap();
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Modifies)
            .count(),
        2
    );

    // Line 3 (`retry(amount, 3)`) came in with the PAY-12 commit.
    let report = engine.why("src/charge.rs:3").unwrap();
    assert_eq!(report.source, "ledger");
    assert_eq!(report.path, "src/charge.rs");
    assert_eq!(report.lines, Some((3, 3)));
    assert_eq!(report.method, "blame");
    assert_eq!(report.commits.len(), 1);
    let commit = &report.commits[0];
    assert_eq!(
        commit.subject,
        "PAY-12: retry charges with a bounded backoff (#7)"
    );
    assert_eq!(commit.author, "Alice Smith");
    assert_eq!(commit.lines, 1);
    assert!(commit.body.contains("one request in a thousand"));

    let pull = &commit.pull_requests[0];
    assert_eq!(pull.key, "pr:ledger#7");
    let dismissed = &pull.decisions[0];
    assert_eq!(
        dismissed.reason.as_deref(),
        Some("Unbounded retries will hammer the gateway.")
    );
    assert!(
        dismissed
            .closed
            .as_deref()
            .unwrap()
            .contains("capped at three")
    );
    assert!(dismissed.closed.as_deref().unwrap().contains("carol"));

    let ticket = &commit.tickets[0];
    assert_eq!(ticket.key, "ticket:PAY-12");
    assert_eq!(ticket.status.as_deref(), Some("done"));
    assert!(ticket.description.as_deref().unwrap().contains("0.1%"));
    assert!(
        commit
            .references
            .iter()
            .any(|r| r.kind == NodeKind::WikiPage && r.title == "Gateway incident review"),
        "{:?}",
        commit.references
    );
    assert!(commit.gaps.is_empty(), "{:?}", commit.gaps);

    let text = render(&report);
    for expected in [
        "PAY-12",
        "pr:ledger#7",
        "capped at three",
        "Gateway incident review",
    ] {
        assert!(text.contains(expected), "{expected} missing from:\n{text}");
    }

    // A symbol resolves to its file and lines; an absolute path works too.
    let by_symbol = engine.why("retry").unwrap();
    assert_eq!(by_symbol.path, "src/charge.rs");
    assert_eq!(by_symbol.commits[0].tickets[0].key, "ticket:PAY-12");
    let absolute = dir.path().join("ledger/src/charge.rs");
    let whole = engine.why(&absolute.to_string_lossy()).unwrap();
    assert_eq!(whole.lines, None);
    assert_eq!(whole.commits.len(), 2);
}

#[test]
fn the_report_says_where_the_trail_ends() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path(), Depth::Full);

    // The README came in with a commit that names no pull request and no ticket.
    let report = engine.why("ledger:README.md").unwrap();
    let commit = &report.commits[0];
    assert_eq!(commit.subject, "Add a README");
    assert!(commit.pull_requests.is_empty() && commit.tickets.is_empty());
    assert_eq!(commit.gaps.len(), 2, "{:?}", commit.gaps);

    // Uncommitted edits are reported, not attributed.
    let path = dir.path().join("ledger/README.md");
    std::fs::write(&path, "# ledger\n\nThe ledger service, edited.\n").unwrap();
    let dirty = engine.why("README.md:3").unwrap();
    assert!(dirty.commits.is_empty());
    assert!(dirty.gaps.iter().any(|gap| gap.contains("not committed")));

    assert!(engine.why("no/such/file.rs").is_err());
}

#[test]
fn history_comes_with_full_depth_only_and_is_ingested_once() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path(), Depth::Map);
    let stats = engine.store().stats().unwrap();
    assert!(!stats.nodes.contains_key("commit"), "{stats:?}");
    // Blame still works at map depth: it reads git, not the graph.
    let report = engine.why("src/charge.rs:3").unwrap();
    assert_eq!(report.commits[0].tickets[0].key, "ticket:PAY-12");

    let deepened = engine
        .deepen(&SourceId::new("ledger"), Depth::Full)
        .unwrap();
    assert!(deepened.units_applied >= 3);
    assert_eq!(engine.store().stats().unwrap().nodes["commit"], 3);
    // A second scan applies no commit again and removes none.
    let again = engine.scan_source(&SourceId::new("ledger")).unwrap();
    assert_eq!(again.units_removed, 0);
    assert_eq!(engine.store().stats().unwrap().nodes["commit"], 3);
}
