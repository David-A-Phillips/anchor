//! `anchor why` target resolution and fallbacks: a source whose working tree is absent, an
//! ambiguous symbol, `source:path` disambiguation, a subdirectory source of a larger repository,
//! and the cases where blame is not available.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anchor::base::{Error, SourceKind};
use anchor::config::{AnchorConfig, Depth, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::router::ProviderKind;
use anchor::engine::why::render;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
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
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn save(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

fn config(dir: &Path, sources: &[(&str, &Path)]) -> AnchorConfig {
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    for (id, location) in sources {
        let mut source = SourceConfig::new(*id, SourceKind::Repo, location.to_string_lossy());
        source.depth = Depth::Full;
        config.sources.push(source);
    }
    config
}

fn open(config: AnchorConfig) -> Arc<Engine> {
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    engine
}

fn invalid(engine: &Engine, target: &str) -> String {
    match engine.why(target) {
        Err(Error::Invalid(message)) => message,
        other => panic!("{target}: expected an invalid-input error, got {other:?}"),
    }
}

/// A repository with two commits touching `src/charge.rs`.
fn ledger(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    write(
        &root.join("src/charge.rs"),
        "pub fn charge(amount: u64) -> u64 {\n    amount\n}\n",
    );
    save(root, "Initial ledger");
    write(
        &root.join("src/charge.rs"),
        "pub fn charge(amount: u64) -> u64 {\n    amount + 1\n}\n",
    );
    save(root, "PAY-12: add the fee (#7)");
}

#[test]
fn a_source_whose_working_tree_is_absent_falls_back_to_the_ingested_history() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("ledger");
    ledger(&repo);
    // Ingest with the working tree present...
    drop(open(config(dir.path(), &[("ledger", &repo)])));

    // ...then open the same data directory on a machine where the repository is not checked out.
    let missing = dir.path().join("not-checked-out");
    let engine = Engine::open(config(dir.path(), &[("ledger", &missing)])).unwrap();
    assert!(engine.is_remote(&anchor::base::SourceId::new("ledger")));
    let report = engine.why("src/charge.rs").unwrap();
    assert_eq!(report.method, "history");
    assert_eq!(report.source, "ledger");
    assert_eq!(report.path, "src/charge.rs");
    assert!(
        report
            .gaps
            .iter()
            .any(|g| g.contains("working tree is not on this machine")),
        "{:?}",
        report.gaps
    );
    let subjects: Vec<&str> = report.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(
        subjects,
        ["PAY-12: add the fee (#7)", "Initial ledger"],
        "newest first"
    );
    assert!(
        report.commits.iter().all(|c| c.lines == 0),
        "history is per file, not per line"
    );
    assert_eq!(report.commits[0].author, "Alice Smith");
    assert!(
        report.commits[0].change_type.is_some() || report.commits[0].subject.contains("PAY-12")
    );
    assert!(render(&report).contains("working tree is not on this machine"));

    // The same holds when asking with an explicit source prefix.
    assert_eq!(engine.why("ledger:src/charge.rs").unwrap().commits.len(), 2);
    // A file the graph never saw is not found.
    assert!(matches!(
        engine.why("src/never-ingested.rs"),
        Err(Error::NotFound(_))
    ));
}

fn repo_with(root: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    for (path, text) in files {
        write(&root.join(path), text);
    }
    save(root, "Initial import");
}

#[test]
fn an_ambiguous_symbol_asks_for_a_path_instead_of_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("svc");
    repo_with(
        &repo,
        &[
            ("src/a.rs", "pub fn process(x: u32) -> u32 {\n    x\n}\n"),
            (
                "src/b.rs",
                "pub fn process(y: u32) -> u32 {\n    y + 1\n}\n",
            ),
            ("src/c.rs", "pub fn unique_helper() -> u32 {\n    7\n}\n"),
        ],
    );
    let engine = open(config(dir.path(), &[("svc", &repo)]));

    let message = invalid(&engine, "process");
    assert!(
        message.contains("several symbols are called `process`"),
        "{message}"
    );
    assert!(
        message.contains("src/a.rs") && message.contains("src/b.rs"),
        "{message}"
    );
    assert!(message.contains("path:line"), "{message}");

    // The advice works: asking with a path and line resolves it.
    let report = engine.why("src/b.rs:2").unwrap();
    assert_eq!(report.path, "src/b.rs");
    assert_eq!(report.lines, Some((2, 2)));
    assert_eq!(report.commits[0].subject, "Initial import");

    // An unambiguous symbol resolves to its file and its own line range.
    let helper = engine.why("unique_helper").unwrap();
    assert_eq!(helper.path, "src/c.rs");
    assert_eq!(helper.lines, Some((1, 3)));
    assert_eq!(helper.method, "blame");

    assert!(matches!(
        engine.why("no_such_symbol_anywhere"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn a_path_present_in_two_repositories_needs_a_source_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    repo_with(&one, &[("src/lib.rs", "pub fn one() {}\n")]);
    repo_with(&two, &[("src/lib.rs", "pub fn two() {}\n")]);
    let engine = open(config(dir.path(), &[("one", &one), ("two", &two)]));

    let message = invalid(&engine, "src/lib.rs");
    assert!(
        message.contains("exists in several repositories"),
        "{message}"
    );
    assert!(
        message.contains("one:src/lib.rs") && message.contains("two:src/lib.rs"),
        "{message}"
    );

    let report = engine.why("two:src/lib.rs").unwrap();
    assert_eq!(
        (report.source.as_str(), report.path.as_str()),
        ("two", "src/lib.rs")
    );
    assert_eq!(report.method, "blame");
    let with_lines = engine.why("one:src/lib.rs:1").unwrap();
    assert_eq!(with_lines.source, "one");
    assert_eq!(with_lines.lines, Some((1, 1)));

    // A prefix that is not a configured source is just part of a path, which does not exist.
    assert!(engine.why("three:src/lib.rs").is_err());
    // A path that exists in only one of them needs no prefix.
    write(&two.join("src/only_two.rs"), "pub fn only() {}\n");
    save(&two, "Add only_two");
    engine.scan_all().unwrap();
    assert_eq!(engine.why("src/only_two.rs").unwrap().source, "two");
}

#[test]
fn a_source_that_is_a_subdirectory_of_a_larger_repository_is_blamed_relative_to_itself() {
    let dir = tempfile::tempdir().unwrap();
    let mono = dir.path().join("mono");
    repo_with(
        &mono,
        &[
            (
                "services/api/src/lib.rs",
                "pub fn serve() {\n    start();\n}\nfn start() {}\n",
            ),
            ("services/web/app.js", "console.log(1)\n"),
        ],
    );
    write(
        &mono.join("services/api/src/lib.rs"),
        "pub fn serve() {\n    start();\n    log();\n}\nfn start() {}\nfn log() {}\n",
    );
    save(&mono, "PAY-5: log requests (#9)");
    // An unrelated change elsewhere in the repository must not appear.
    write(&mono.join("services/web/app.js"), "console.log(2)\n");
    save(&mono, "Touch web");

    let api = mono.join("services/api");
    let engine = open(config(dir.path(), &[("api", &api)]));
    let report = engine.why("src/lib.rs:3").unwrap();
    assert_eq!(report.source, "api");
    assert_eq!(
        report.path, "src/lib.rs",
        "relative to the source, not the repository"
    );
    assert_eq!(report.method, "blame");
    assert_eq!(report.commits.len(), 1);
    assert_eq!(report.commits[0].subject, "PAY-5: log requests (#9)");
    assert_eq!(report.commits[0].lines, 1);

    let whole = engine.why("src/lib.rs").unwrap();
    let subjects: Vec<&str> = whole.commits.iter().map(|c| c.subject.as_str()).collect();
    assert!(subjects.contains(&"Initial import") && subjects.contains(&"PAY-5: log requests (#9)"));
    assert!(!subjects.contains(&"Touch web"));

    // By absolute path on disk, as an editor integration would pass it.
    let by_disk = engine
        .why(&format!("{}:3", api.join("src/lib.rs").display()))
        .unwrap();
    assert_eq!(
        (by_disk.source.as_str(), by_disk.path.as_str()),
        ("api", "src/lib.rs")
    );
}

#[test]
fn when_blame_is_not_available_the_file_history_is_used_and_the_gap_is_stated() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("svc");
    repo_with(&repo, &[("src/a.rs", "one\ntwo\nthree\n")]);
    let engine = open(config(dir.path(), &[("svc", &repo)]));

    // A range beyond the end of the file.
    let beyond = engine.why("src/a.rs:40-50").unwrap();
    assert_eq!(beyond.method, "history");
    assert!(
        beyond
            .gaps
            .iter()
            .any(|g| g.contains("blame is not available")),
        "{:?}",
        beyond.gaps
    );
    assert_eq!(beyond.commits.len(), 1, "the file's history still answers");

    // A file that exists on disk but was never committed.
    write(&repo.join("src/new.rs"), "fresh\n");
    let untracked = engine.why("src/new.rs").unwrap();
    assert_eq!(untracked.method, "history");
    assert!(untracked.commits.is_empty());
    assert!(
        untracked
            .gaps
            .iter()
            .any(|g| g.contains("no commit was found"))
    );
    assert!(render(&untracked).contains("no commit was found"));

    // Lines changed in the working tree but not committed are called out, not blamed on nobody.
    write(&repo.join("src/a.rs"), "one\ntwo\nthree\nfour\n");
    let dirty = engine.why("src/a.rs").unwrap();
    assert!(
        dirty.gaps.iter().any(|g| g.contains("not committed yet")),
        "{:?}",
        dirty.gaps
    );
    assert_eq!(dirty.commits.len(), 1);
    assert_eq!(dirty.commits[0].lines, 3);
}

#[test]
fn degenerate_targets_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("svc");
    repo_with(&repo, &[("src/a.rs", "pub fn a() {}\n")]);
    let engine = open(config(dir.path(), &[("svc", &repo)]));
    for target in [
        "",
        "   ",
        ":",
        "::",
        "src/a.rs:",
        "src/a.rs:0",
        "src/a.rs:5-2",
        "src/a.rs:x-y",
        "../../etc/passwd",
        "svc:",
        "a\0b",
        "%",
        "\"",
        "*",
    ] {
        // Whatever they resolve to, they must neither panic nor report a path outside the source.
        if let Ok(report) = engine.why(target) {
            assert!(!report.path.starts_with(".."), "{target}: {}", report.path);
        }
    }
    assert!(engine.why("../../etc/passwd").is_err());
    // A zero or inverted line range is ignored rather than passed to git.
    assert_eq!(
        engine.why("src/a.rs:0").map(|r| r.lines).ok().flatten(),
        None
    );
}
