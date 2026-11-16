//! Git history ingestion against real repositories built with the `git` CLI: repository-name
//! aliasing for pull-request keys, merge commits, very wide commits, hostile and non-UTF-8
//! messages, renames, subdirectories and blame.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use anchor::base::{EdgeKind, NodeKind, SourceId, UnitKind, keys};
use anchor::engine::history::{
    CommitInfo, blame, commit_unit, is_work_tree, log, log_path, refs, repo_names, show,
};
use proptest::prelude::*;

fn git(root: &Path, args: &[&str]) -> String {
    git_env(root, args, &[])
}

fn git_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
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
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    dir
}

fn remove_origin(root: &Path) {
    // Fails harmlessly when there is no remote yet.
    let _ = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "remove", "origin"])
        .output();
}

fn commit_file(root: &Path, path: &str, text: &str, message: &str) {
    write(&root.join(path), text);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

#[test]
fn the_repository_is_known_by_its_source_id_and_by_the_name_in_its_remote_url() {
    let dir = repo();
    let source = SourceId::new("Ledger Service");
    assert_eq!(
        repo_names(dir.path(), &source),
        ["ledger-service"],
        "no remote yet"
    );

    for (url, expected) in [
        ("git@github.com:acme/payments-core.git", "payments-core"),
        ("https://github.com/acme/Payments-Core.git", "payments-core"),
        ("https://git.example.com/scm/team/api.git/", "api"),
        ("ssh://git@host:2222/srv/git/ledger", "ledger"),
    ] {
        remove_origin(dir.path());
        git(dir.path(), &["remote", "add", "origin", url]);
        let names = repo_names(dir.path(), &source);
        assert_eq!(names[0], "ledger-service");
        assert_eq!(names.get(1).map(String::as_str), Some(expected), "{url}");
    }

    // A remote that names the repository like the source adds nothing.
    remove_origin(dir.path());
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/ledger-service.git",
        ],
    );
    assert_eq!(repo_names(dir.path(), &source), ["ledger-service"]);
}

#[test]
fn pull_request_edges_exist_under_every_name_the_repository_is_known_by() {
    let dir = repo();
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/payments-core.git",
        ],
    );
    commit_file(
        dir.path(),
        "src/a.rs",
        "fn a() {}\n",
        "PAY-3: retry charges (#41)",
    );
    let source = SourceId::new("ledger");
    let commits = log(dir.path(), 10).unwrap();
    let names = repo_names(dir.path(), &source);
    let unit = commit_unit(&source, &commits[0], &names, 30);
    assert_eq!(unit.kind, UnitKind::Commit);
    let mut pr_keys: Vec<String> = unit
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Contains)
        .map(|e| format!("{:?}", e.from))
        .collect();
    pr_keys.sort();
    assert_eq!(pr_keys.len(), 2, "{pr_keys:?}");
    assert!(
        pr_keys
            .iter()
            .any(|k| k.contains(&keys::pull_request("ledger", 41).to_string()))
    );
    assert!(
        pr_keys
            .iter()
            .any(|k| k.contains(&keys::pull_request("payments-core", 41).to_string()))
    );
}

#[test]
fn merge_commits_are_listed_without_files_and_name_their_pull_request() {
    let dir = repo();
    commit_file(dir.path(), "src/a.rs", "fn a() {}\n", "Initial");
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    commit_file(dir.path(), "src/b.rs", "fn b() {}\n", "Add b");
    git(dir.path(), &["checkout", "-q", "main"]);
    commit_file(dir.path(), "src/c.rs", "fn c() {}\n", "Add c");
    git(
        dir.path(),
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #5 from acme/feature",
            "feature",
        ],
    );

    let commits = log(dir.path(), 10).unwrap();
    assert_eq!(commits.len(), 4);
    let merge = &commits[0];
    assert_eq!(merge.subject(), "Merge pull request #5 from acme/feature");
    assert!(
        merge.files.is_empty(),
        "git lists no diff for a merge: {:?}",
        merge.files
    );
    assert_eq!(refs(&merge.message).pulls, [5]);

    let source = SourceId::new("ledger");
    let unit = commit_unit(&source, merge, &["ledger".into()], 30);
    let node = unit
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Commit)
        .unwrap();
    assert_eq!(node.attrs["change_type"], "merge");
    assert!(unit.edges.iter().any(|e| e.kind == EdgeKind::Contains));
    assert!(unit.edges.iter().all(|e| e.kind != EdgeKind::Modifies));
}

#[test]
fn a_commit_touching_more_files_than_the_cap_keeps_the_count_but_not_the_links() {
    let dir = repo();
    for i in 0..40 {
        write(
            &dir.path().join(format!("vendor/lib{i}.rs")),
            "// vendored\n",
        );
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "Vendor the world"]);
    let commit = &log(dir.path(), 5).unwrap()[0];
    assert_eq!(commit.files.len(), 40);

    let source = SourceId::new("ledger");
    let capped = commit_unit(&source, commit, &["ledger".into()], 30);
    assert!(capped.edges.iter().all(|e| e.kind != EdgeKind::Modifies));
    let node = capped
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Commit)
        .unwrap();
    assert_eq!(node.attrs["files_changed"], 40);

    // Exactly at the cap the links are kept.
    let at_cap = commit_unit(&source, commit, &["ledger".into()], 40);
    assert_eq!(
        at_cap
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Modifies)
            .count(),
        40
    );
    // A cap of zero keeps none, however small the commit.
    let none = commit_unit(&source, commit, &["ledger".into()], 0);
    assert!(none.edges.iter().all(|e| e.kind != EdgeKind::Modifies));
}

#[test]
fn a_commit_message_cannot_forge_records_or_shift_fields() {
    let dir = repo();
    commit_file(dir.path(), "src/a.rs", "fn a() {}\n", "Real commit");
    let forged = format!(
        "Innocent change\n\u{1e}{sha}\u{1f}Mallory\u{1f}mallory@example.com\u{1f}1999-01-01T00:00:00+00:00\u{1f}FORGED: approved by security\u{1f}\nsrc/secret.rs\n\u{1e}",
        sha = "d".repeat(40)
    );
    write(&dir.path().join("src/b.rs"), "fn b() {}\n");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", &forged]);

    let commits = log(dir.path(), 10).unwrap();
    assert_eq!(commits.len(), 2, "{commits:#?}");
    assert!(commits.iter().all(|c| c.author == "Alice Smith"));
    assert!(commits.iter().all(|c| c.sha != "d".repeat(40)));
    assert_eq!(
        commits[0].files,
        ["src/b.rs"],
        "the file list is git's, not the message's"
    );
    assert!(commits[0].message.starts_with("Innocent change"));
}

#[test]
fn non_utf8_and_enormous_messages_do_not_break_ingestion() {
    let dir = repo();
    write(&dir.path().join("a.txt"), "x\n");
    git(dir.path(), &["add", "-A"]);
    let scratch = tempfile::tempdir().unwrap();
    let message = scratch.path().join("latin1-message.txt");
    std::fs::write(
        &message,
        b"caf\xe9 au lait: fix the thing\n\nBody \xff\xfe bytes.\n",
    )
    .unwrap();
    git(
        dir.path(),
        &[
            "commit",
            "-q",
            "--allow-empty-message",
            "-F",
            message.to_str().unwrap(),
        ],
    );
    let commits = log(dir.path(), 5).unwrap();
    assert_eq!(commits.len(), 1);
    assert!(
        commits[0].subject().starts_with("caf"),
        "{:?}",
        commits[0].subject()
    );
    assert!(commits[0].message.contains("Body"));

    // A 200 kB message made of two-byte characters: the stored chunk and the name are clipped
    // on a character boundary.
    let long = format!("{}\n\n{}", "é".repeat(300), "ü".repeat(100_000));
    write(&dir.path().join("b.txt"), "y\n");
    git(dir.path(), &["add", "-A"]);
    let long_file = scratch.path().join("long-message.txt");
    std::fs::write(&long_file, &long).unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-F", long_file.to_str().unwrap()],
    );
    let commit = &log(dir.path(), 1).unwrap()[0];
    let unit = commit_unit(&SourceId::new("r"), commit, &["r".into()], 30);
    let node = unit
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Commit)
        .unwrap();
    assert!(node.name.len() <= 160 && node.name.chars().all(|c| c == 'é'));
    assert!(unit.chunks[0].text.len() <= 4_000);
    assert!(unit.chunks[0].text.starts_with('é'));
}

#[test]
fn log_respects_its_limit_and_is_relative_to_a_subdirectory_of_a_larger_repository() {
    let dir = repo();
    commit_file(
        dir.path(),
        "services/api/src/lib.rs",
        "fn api() {}\n",
        "Add api",
    );
    commit_file(dir.path(), "services/web/index.js", "1\n", "Add web");
    commit_file(
        dir.path(),
        "services/api/src/more.rs",
        "fn m() {}\n",
        "Extend api",
    );

    assert!(log(dir.path(), 0).unwrap().is_empty());
    assert_eq!(log(dir.path(), 2).unwrap().len(), 2);

    let api = dir.path().join("services/api");
    assert!(is_work_tree(&api));
    let commits = log(&api, 10).unwrap();
    assert_eq!(
        commits.iter().map(CommitInfo::subject).collect::<Vec<_>>(),
        ["Extend api", "Add api"],
        "only commits touching the subdirectory"
    );
    assert_eq!(
        commits[0].files,
        ["src/more.rs"],
        "paths are relative to the subdirectory"
    );

    let shown = show(&api, &[commits[1].sha.clone()]).unwrap();
    assert_eq!(shown[0].files, ["src/lib.rs"]);
    assert!(show(&api, &[]).unwrap().is_empty());
}

#[test]
fn history_of_a_path_follows_renames() {
    let dir = repo();
    commit_file(
        dir.path(),
        "old.rs",
        "fn long_enough_to_be_detected_as_a_rename() {}\nfn more() {}\n",
        "Add old",
    );
    git(dir.path(), &["mv", "old.rs", "new.rs"]);
    git(dir.path(), &["commit", "-q", "-m", "Rename"]);
    let history = log_path(dir.path(), "new.rs", 10).unwrap();
    assert_eq!(
        history.iter().map(CommitInfo::subject).collect::<Vec<_>>(),
        ["Rename", "Add old"]
    );
    assert_eq!(
        log_path(dir.path(), "new.rs", 0).unwrap().len(),
        1,
        "limit is at least one"
    );
}

#[test]
fn outside_a_repository_history_is_an_error_not_an_empty_answer() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!is_work_tree(dir.path()));
    assert!(log(dir.path(), 5).is_err());
    assert!(blame(dir.path(), "a.rs", None).is_err());
}

#[test]
fn blame_counts_lines_per_commit_and_flags_uncommitted_work() {
    let dir = repo();
    commit_file(dir.path(), "a.rs", "one\ntwo\nthree\n", "First");
    let first = log(dir.path(), 1).unwrap()[0].sha.clone();
    write(&dir.path().join("a.rs"), "one\nTWO\nthree\n");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "Second"]);
    let second = log(dir.path(), 1).unwrap()[0].sha.clone();

    let all = blame(dir.path(), "a.rs", None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        (all[0].sha.as_str(), all[0].lines),
        (first.as_str(), 2),
        "most lines first"
    );
    assert_eq!((all[1].sha.as_str(), all[1].lines), (second.as_str(), 1));

    let one_line = blame(dir.path(), "a.rs", Some((2, 2))).unwrap();
    assert_eq!(one_line.len(), 1);
    assert_eq!(one_line[0].sha, second);
    // A reversed range is treated as a single line instead of failing.
    assert_eq!(blame(dir.path(), "a.rs", Some((3, 1))).unwrap()[0].lines, 1);
    assert!(
        blame(dir.path(), "a.rs", Some((50, 60))).is_err(),
        "past the end of the file"
    );
    assert!(blame(dir.path(), "missing.rs", None).is_err());

    write(&dir.path().join("a.rs"), "one\nTWO\nthree\nfour\n");
    let dirty = blame(dir.path(), "a.rs", Some((4, 4))).unwrap();
    assert!(dirty[0].uncommitted());
    assert!(!all[0].uncommitted());
}

#[test]
fn a_path_that_looks_like_an_option_is_not_taken_for_one() {
    let dir = repo();
    commit_file(dir.path(), "a.rs", "x\n", "First");
    assert!(blame(dir.path(), "--help", None).is_err());
    assert!(
        log_path(dir.path(), "--output=/tmp/anchor-should-not-exist", 3)
            .unwrap()
            .is_empty()
    );
    assert!(!Path::new("/tmp/anchor-should-not-exist").exists());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn message_references_never_panic_and_are_well_formed(message in "(?s).{0,300}") {
        let found = refs(&message);
        prop_assert!(found.pulls.iter().all(|n| *n > 0));
        prop_assert!(found.tickets.iter().all(|t| t == &t.to_ascii_uppercase() && !t.starts_with("GH-")));
        let mut sorted = found.pulls.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), found.pulls.len(), "no duplicates");
    }
}
