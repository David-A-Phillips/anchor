//! Git history: commits as graph nodes, and line-level blame on demand.
//!
//! History is what connects a line of code to the reason it exists: line → commit → pull request
//! → review discussion → ticket → design page. Commits are read with the `git` command line (no
//! library, no network) and become [`NodeKind::Commit`] nodes linked to their author, the files
//! they changed, and the tickets and pull requests their message names. Blame is never stored:
//! it is computed for the lines asked about (see [`crate::engine::why`]).
//!
//! # Graph contribution
//!
//! * **Commit** node `keys::commit(source, sha)`, named after the subject line, with `sha`,
//!   `created_at` (author date) and `files_changed`; the full message is its text.
//! * `commit --AuthoredBy--> person` (asserted from the author name and e-mail).
//! * `commit --Modifies--> file` by key, for up to [`HistoryConfig::max_files_per_commit`] files.
//! * `ticket --Tracks--> commit` for every tracker key in the message (`PAY-123`).
//! * `pull_request --Contains--> commit` for every pull-request reference (`#123`, `(#123)`,
//!   `Merge pull request #123`), under each name the repository is known by.
//!
//! Edges to tickets and pull requests are asserted by key, so they connect whenever the tracker
//! or review export is ingested, before or after the history.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::ingest::mentions::extract_mentions;
use super::ingest::party::PersonIdentity;
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, NodeAssertion, NodeKind, Result, SourceId, Unit,
    UnitKind, attrs, keys,
};

/// Commit attribute: the full hash.
pub const ATTR_SHA: &str = "sha";
/// Commit attribute: what kind of change the message announces (see [`change_type`]).
pub const ATTR_CHANGE_TYPE: &str = "change_type";
/// Commit attribute: number of files the commit changed.
pub const ATTR_FILES_CHANGED: &str = "files_changed";

const MAX_MESSAGE_CHARS: usize = 4_000;
const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';
const FORMAT: &str = "--pretty=format:%x1e%H%x1f%an%x1f%ae%x1f%aI%x1f%B%x1f";

/// Git history settings (`[history]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// Ingest commits of repositories held at full depth.
    pub enabled: bool,
    /// Most recent commits to ingest per repository; older ones are still reachable through
    /// blame, read from git when asked.
    pub max_commits: usize,
    /// Files linked to one commit; larger commits (vendoring, mass renames) keep only the count.
    pub max_files_per_commit: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_commits: 2_000,
            max_files_per_commit: 30,
        }
    }
}

/// One commit as read from git.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full hash.
    pub sha: String,
    /// Author name.
    pub author: String,
    /// Author e-mail.
    pub email: String,
    /// Author date, RFC 3339.
    pub date: String,
    /// Full message.
    pub message: String,
    /// Paths changed, relative to the directory git was run in.
    pub files: Vec<String>,
}

impl CommitInfo {
    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }

    /// The abbreviated hash.
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(10)]
    }
}

/// Runs `git` in `root` and returns its standard output.
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        // Never prompt, never page, never read the user's aliases for these plumbing calls.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .map_err(|err| Error::other(format!("cannot run git: {err}")))?;
    if !output.status.success() {
        return Err(Error::other(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `true` when `root` is inside a git work tree (and `git` is installed).
pub fn is_work_tree(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out.trim() == "true")
}

fn parse_log(output: &str) -> Vec<CommitInfo> {
    output
        .split(RECORD)
        .filter_map(|record| {
            let mut fields = record.split(FIELD);
            let sha = fields.next()?.trim();
            if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let author = fields.next()?.trim().to_owned();
            let email = fields.next()?.trim().to_owned();
            let date = fields.next()?.trim().to_owned();
            let message = fields.next()?.trim().to_owned();
            let files = fields
                .next()
                .unwrap_or("")
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(keys::normalize_path)
                .collect();
            Some(CommitInfo {
                sha: sha.to_owned(),
                author,
                email,
                date,
                message,
                files,
            })
        })
        .collect()
}

/// The most recent `max` commits touching `root` (a repository or a directory inside one),
/// newest first, with paths relative to `root`.
pub fn log(root: &Path, max: usize) -> Result<Vec<CommitInfo>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let count = format!("--max-count={max}");
    let output = git(
        root,
        &[
            "log",
            &count,
            "--relative",
            "--name-only",
            FORMAT,
            "--",
            ".",
        ],
    )?;
    Ok(parse_log(&output))
}

/// The given commits, in the order git lists them, with paths relative to `root`.
pub fn show(root: &Path, shas: &[String]) -> Result<Vec<CommitInfo>> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "log",
        "--no-walk=unsorted",
        "--relative",
        "--name-only",
        FORMAT,
    ];
    args.extend(shas.iter().map(String::as_str));
    Ok(parse_log(&git(root, &args)?))
}

/// The most recent `max` commits that changed `path` (relative to `root`), newest first.
pub fn log_path(root: &Path, path: &str, max: usize) -> Result<Vec<CommitInfo>> {
    let count = format!("--max-count={}", max.max(1));
    let output = git(
        root,
        &[
            "log",
            &count,
            "--relative",
            "--name-only",
            "--follow",
            FORMAT,
            "--",
            path,
        ],
    )?;
    Ok(parse_log(&output))
}

/// Names the repository is known by, as slugs: the source id and, when there is an `origin`
/// remote, the repository name in its URL. Pull requests are keyed by repository name.
pub fn repo_names(root: &Path, source: &SourceId) -> Vec<String> {
    let mut names = vec![keys::slugify(source.as_str())];
    if let Ok(url) = git(root, &["config", "--get", "remote.origin.url"]) {
        let name = url
            .trim()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .map(keys::slugify)
            .unwrap_or_default();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// What a commit message refers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refs {
    /// Tracker keys (`PAY-123`), upper case, in order of appearance.
    pub tickets: Vec<String>,
    /// Pull-request numbers.
    pub pulls: Vec<u64>,
}

static PULL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s(\[,;:])(?:pull request |pr |gh-)?#(\d{1,7})\b|\bgh-(\d{1,7})\b")
        .expect("static regex is valid")
});

/// Reads tracker keys and pull-request numbers out of a commit message.
pub fn refs(message: &str) -> Refs {
    let mut out = Refs::default();
    for mention in extract_mentions(message) {
        if mention.lookup.kinds.first() == Some(&NodeKind::Ticket) {
            let key = mention.lookup.text.to_ascii_uppercase();
            // `GH-123` is GitHub's spelling of a pull request or issue, not a tracker key.
            if !key.starts_with("GH-") && !out.tickets.contains(&key) {
                out.tickets.push(key);
            }
        }
    }
    for found in PULL.captures_iter(message) {
        if let Some(number) = found
            .get(1)
            .or(found.get(2))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            && number > 0
            && !out.pulls.contains(&number)
        {
            out.pulls.push(number);
        }
    }
    out
}

/// What kind of change a commit message announces: `revert`, `security`, `fix`, `performance`,
/// `feature`, `refactor`, `docs`, `test`, `build`, `chore` or `merge`. Read from conventional
/// commit prefixes (`fix:`, `feat(scope):`) and, failing that, from the subject's wording.
/// `None` when the subject gives no hint.
pub fn change_type(message: &str) -> Option<&'static str> {
    let subject = message.lines().next().unwrap_or("").trim().to_lowercase();
    if subject.starts_with("revert") {
        return Some("revert");
    }
    if subject.starts_with("merge ") {
        return Some("merge");
    }
    let prefix = subject
        .split_once(':')
        .map(|(prefix, _)| {
            prefix
                .split('(')
                .next()
                .unwrap_or("")
                .trim_end_matches('!')
                .trim()
        })
        .unwrap_or("");
    let by_prefix = match prefix {
        "fix" | "bugfix" | "hotfix" => Some("fix"),
        "feat" | "feature" => Some("feature"),
        "perf" => Some("performance"),
        "refactor" => Some("refactor"),
        "docs" | "doc" => Some("docs"),
        "test" | "tests" => Some("test"),
        "build" | "ci" | "deps" => Some("build"),
        "chore" | "style" => Some("chore"),
        "security" | "sec" => Some("security"),
        _ => None,
    };
    if by_prefix.is_some() {
        return by_prefix;
    }
    let padded = format!(" {subject} ");
    let has = |words: &[&str]| words.iter().any(|word| padded.contains(word));
    if has(&[" cve-", " security ", " vulnerab", " xss ", " injection "]) {
        Some("security")
    } else if has(&[
        " fix ",
        " fixes ",
        " fixed ",
        " hotfix",
        " bug ",
        " regression",
        " workaround",
    ]) {
        Some("fix")
    } else if has(&[" performance", " speed up", " faster ", " optimi"]) {
        Some("performance")
    } else if has(&[
        " refactor",
        " rename ",
        " cleanup",
        " clean up",
        " simplify",
    ]) {
        Some("refactor")
    } else if has(&[" bump ", " upgrade ", " update dependenc"]) {
        Some("build")
    } else if has(&[" add ", " adds ", " introduce", " implement", " support "]) {
        Some("feature")
    } else {
        None
    }
}

/// The author as a person: GitHub's no-reply addresses carry the account handle, not a mailbox.
pub fn author_identity(name: &str, email: &str) -> PersonIdentity {
    let email = email.trim().to_ascii_lowercase();
    if let Some(local) = email.strip_suffix("@users.noreply.github.com") {
        let handle = local.rsplit('+').next().unwrap_or(local);
        return PersonIdentity::new(Some(name), Some(handle), None);
    }
    PersonIdentity::new(Some(name), None, Some(&email))
}

fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// The ingestion unit of one commit.
pub fn commit_unit(
    source: &SourceId,
    commit: &CommitInfo,
    repo_names: &[String],
    max_files: usize,
) -> Unit {
    let key = keys::commit(source, &commit.sha);
    let mut unit = Unit::new(
        keys::unit(source, UnitKind::Commit, &commit.sha),
        source.clone(),
        UnitKind::Commit,
    );
    let subject = commit.subject();
    let name = if subject.is_empty() {
        commit.short().to_owned()
    } else {
        clip(subject, 160).to_owned()
    };
    unit.add_node(
        NodeAssertion::new(NodeKind::Commit, key.clone(), name)
            .alias(commit.sha.as_str())
            .alias(commit.short())
            .attr(ATTR_SHA, commit.sha.as_str())
            .attr(ATTR_FILES_CHANGED, commit.files.len() as u64)
            .attr_opt(ATTR_CHANGE_TYPE, change_type(&commit.message))
            .attr_opt(
                attrs::CREATED_AT,
                (!commit.date.is_empty()).then(|| commit.date.clone()),
            ),
    );
    unit.add_chunk(
        &key,
        ChunkKind::Text,
        clip(&commit.message, MAX_MESSAGE_CHARS),
    );
    if let Some(person) = author_identity(&commit.author, &commit.email).assertion() {
        let person = unit.add_node(person);
        unit.add_edge(EdgeAssertion::new(&key, EdgeKind::AuthoredBy, &person));
    }
    if commit.files.len() <= max_files {
        for path in &commit.files {
            unit.add_edge(EdgeAssertion::new(
                &key,
                EdgeKind::Modifies,
                keys::file(source, path),
            ));
        }
    }
    let refs = refs(&commit.message);
    for ticket in &refs.tickets {
        unit.add_edge(EdgeAssertion::new(
            keys::ticket(ticket),
            EdgeKind::Tracks,
            &key,
        ));
    }
    for number in &refs.pulls {
        for repo in repo_names {
            unit.add_edge(EdgeAssertion::new(
                keys::pull_request(repo, *number),
                EdgeKind::Contains,
                &key,
            ));
        }
    }
    unit.finish()
}

/// Lines attributed to one commit by blame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blamed {
    /// Full hash; all zeros for lines not committed yet.
    pub sha: String,
    /// Number of lines in the asked range last changed by this commit.
    pub lines: u32,
}

impl Blamed {
    /// `true` for working-tree changes that are not committed.
    pub fn uncommitted(&self) -> bool {
        self.sha.chars().all(|c| c == '0')
    }
}

/// Blames `path` (relative to `root`), optionally only lines `range` (1-based, inclusive):
/// the commits that last changed those lines, most lines first.
pub fn blame(root: &Path, path: &str, range: Option<(u32, u32)>) -> Result<Vec<Blamed>> {
    let mut args = vec!["blame".to_owned(), "--porcelain".to_owned()];
    if let Some((start, end)) = range {
        args.push(format!("-L{},{}", start.max(1), end.max(start).max(1)));
    }
    args.push("--".to_owned());
    args.push(path.to_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git(root, &args)?;
    let mut counts: BTreeMap<String, (usize, u32)> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in output.lines() {
        if line.starts_with('\t') {
            if let Some(sha) = &current {
                let next = counts.len();
                counts.entry(sha.clone()).or_insert((next, 0)).1 += 1;
            }
            continue;
        }
        let token = line.split(' ').next().unwrap_or("");
        if token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            current = Some(token.to_owned());
        }
    }
    let mut out: Vec<(usize, Blamed)> = counts
        .into_iter()
        .map(|(sha, (order, lines))| (order, Blamed { sha, lines }))
        .collect();
    out.sort_by(|a, b| b.1.lines.cmp(&a.1.lines).then(a.0.cmp(&b.0)));
    Ok(out.into_iter().map(|(_, blamed)| blamed).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_yield_tickets_and_pull_requests() {
        let found = refs(
            "PAY-12: retry charges with backoff (#7)\n\nFollows up on pay-9 and GH-31; see RFC-2616.",
        );
        assert_eq!(found.tickets, ["PAY-12"]);
        assert_eq!(found.pulls, [7, 31]);
        assert_eq!(
            refs("Merge pull request #204 from acme/fix-ledger").pulls,
            [204]
        );
        assert_eq!(
            refs("Bump version to 1.2.3; color #fff").pulls,
            Vec::<u64>::new()
        );
        assert_eq!(refs("issue#12 is unrelated"), Refs::default());
    }

    #[test]
    fn change_types_are_read_from_prefixes_and_wording() {
        assert_eq!(
            change_type("Revert \"PAY-12: retry charges\""),
            Some("revert")
        );
        assert_eq!(
            change_type("fix(ledger): off-by-one in totals"),
            Some("fix")
        );
        assert_eq!(change_type("feat!: new settlement API"), Some("feature"));
        assert_eq!(
            change_type("Alerting: Fix panic on empty rule group (#9)"),
            Some("fix")
        );
        assert_eq!(change_type("Bump golang.org/x/net to 0.30"), Some("build"));
        assert_eq!(change_type("Merge pull request #7 from a/b"), Some("merge"));
        assert_eq!(
            change_type("Patch CVE-2025-1234 in the parser"),
            Some("security")
        );
        assert_eq!(
            change_type("Dashboards: Add panel actions popover"),
            Some("feature")
        );
        assert_eq!(change_type("wip"), None);
    }

    #[test]
    fn log_output_is_parsed_into_commits() {
        let output = format!(
            "{RECORD}{sha}{FIELD}Alice Smith{FIELD}alice@example.com{FIELD}2025-03-01T10:00:00+00:00{FIELD}PAY-1: first\n\nBody line.\n{FIELD}\nsrc/a.rs\nREADME.md\n\n{RECORD}{sha2}{FIELD}Bob{FIELD}1+bob@users.noreply.github.com{FIELD}2025-03-02T10:00:00+00:00{FIELD}second{FIELD}",
            sha = "a".repeat(40),
            sha2 = "b".repeat(40)
        );
        let commits = parse_log(&output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject(), "PAY-1: first");
        assert_eq!(commits[0].message, "PAY-1: first\n\nBody line.");
        assert_eq!(commits[0].files, ["src/a.rs", "README.md"]);
        assert_eq!(commits[0].short(), "aaaaaaaaaa");
        assert!(commits[1].files.is_empty());
        let bob = author_identity(&commits[1].author, &commits[1].email);
        assert_eq!(bob.handle.as_deref(), Some("bob"));
        assert_eq!(bob.email, None);
    }

    #[test]
    fn a_commit_unit_links_author_files_tickets_and_pull_requests() {
        let source = SourceId::new("ledger");
        let commit = CommitInfo {
            sha: "c".repeat(40),
            author: "Alice Smith".into(),
            email: "alice@example.com".into(),
            date: "2025-03-01T10:00:00+00:00".into(),
            message: "PAY-12: retry charges (#7)\n\nBecause the gateway drops requests.".into(),
            files: vec!["src/charge.rs".into()],
        };
        let unit = commit_unit(
            &source,
            &commit,
            &["ledger".into(), "ledger-api".into()],
            30,
        );
        let key = keys::commit(&source, &commit.sha);
        assert_eq!(unit.kind, UnitKind::Commit);
        assert!(
            unit.nodes
                .iter()
                .any(|n| n.key == key && n.name == "PAY-12: retry charges (#7)")
        );
        assert!(unit.nodes.iter().any(|n| n.kind == NodeKind::Person));
        let kinds: Vec<EdgeKind> = unit.edges.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == EdgeKind::Modifies).count(),
            1
        );
        assert_eq!(kinds.iter().filter(|k| **k == EdgeKind::Tracks).count(), 1);
        assert_eq!(
            kinds.iter().filter(|k| **k == EdgeKind::Contains).count(),
            2
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == EdgeKind::AuthoredBy).count(),
            1
        );
        assert_eq!(unit.chunks.len(), 1);

        // A commit touching more files than the cap keeps the count but not the links.
        let mut wide = commit.clone();
        wide.files = (0..5).map(|i| format!("vendor/{i}.rs")).collect();
        let unit = commit_unit(&source, &wide, &["ledger".into()], 3);
        assert!(unit.edges.iter().all(|e| e.kind != EdgeKind::Modifies));
    }
}
