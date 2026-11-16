//! `anchor why`: the trail from a line of code to the reason it is the way it is.
//!
//! Given a file (optionally with a line range) or a symbol, the engine blames the lines, and for
//! each responsible commit follows the trail through the graph: the pull request that carried
//! it, the review decisions and threads on that pull request (including dismissed ones and why),
//! the tickets it tracks with their state and blockers, and the pages that discuss any of them.
//! Everything is read from git and the graph; no model is involved. Where the trail ends early
//! (a commit without a pull request, a ticket that was never ingested) the report says so.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::core::Engine;
use super::history::{self, CommitInfo};
use super::store::{EdgeRecord, NodeRecord};
use crate::base::{
    EdgeKind, Error, Lookup, NodeId, NodeKind, Result, SourceId, SourceKind, TrustLevel, attrs,
    keys,
};
use crate::engine::store::Resolution;

const MAX_COMMITS: usize = 5;
const MAX_ITEMS: usize = 8;
const SNIPPET: usize = 400;

/// A review decision on a pull request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// What was decided and by whom, as recorded ("Review of x#7 by bob: dismissed").
    pub title: String,
    /// `approved`, `changes_requested`, `rejected`, ...
    pub decision: Option<String>,
    /// The reviewer's stated reason.
    pub reason: Option<String>,
    /// Why the decision was later dismissed or closed, and by whom.
    pub closed: Option<String>,
}

/// A discussion thread on a pull request, ticket or page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Discussion {
    /// The thread.
    pub title: String,
    /// `open`, `resolved` or `dismissed`.
    pub status: Option<String>,
    /// The text the thread is anchored to.
    pub anchor: Option<String>,
    /// Why it was closed, and by whom.
    pub closed: Option<String>,
    /// The opening text.
    pub text: Option<String>,
}

/// A pull request on the trail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PullRequestTrail {
    /// Node key.
    pub key: String,
    /// Title.
    pub title: String,
    /// State.
    pub status: Option<String>,
    /// Link.
    pub url: Option<String>,
    /// What the pull request says about itself.
    pub description: Option<String>,
    /// Review decisions.
    pub decisions: Vec<Decision>,
    /// Review threads.
    pub discussions: Vec<Discussion>,
}

/// A ticket on the trail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketTrail {
    /// Node key.
    pub key: String,
    /// Title.
    pub title: String,
    /// State.
    pub status: Option<String>,
    /// Link.
    pub url: Option<String>,
    /// What the ticket asks for.
    pub description: Option<String>,
    /// What blocks it, with reasons.
    pub blocked_by: Vec<String>,
}

/// A page or thread that discusses something on the trail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// Node key.
    pub key: String,
    /// Kind of node.
    pub kind: NodeKind,
    /// Title.
    pub title: String,
    /// What it refers to on the trail.
    pub about: String,
    /// Trust of the source.
    pub trust: TrustLevel,
    /// Link.
    pub url: Option<String>,
    /// Why it was closed, for dismissed or resolved threads.
    pub closed: Option<String>,
}

/// One commit and everything it leads to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitTrail {
    /// Full hash.
    pub sha: String,
    /// Author name.
    pub author: String,
    /// Author date.
    pub date: String,
    /// Subject line.
    pub subject: String,
    /// What kind of change the message announces (`fix`, `revert`, `feature`, ...), if any.
    pub change_type: Option<String>,
    /// Rest of the message.
    pub body: String,
    /// Lines of the asked range last changed by this commit (0 when blame was not available).
    pub lines: u32,
    /// Pull requests that carried the commit.
    pub pull_requests: Vec<PullRequestTrail>,
    /// Tickets the commit or its pull requests track.
    pub tickets: Vec<TicketTrail>,
    /// Pages and threads discussing the tickets or pull requests.
    pub references: Vec<Reference>,
    /// Where this commit's trail ends early.
    pub gaps: Vec<String>,
}

/// The answer to "why is this code the way it is?".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhyReport {
    /// What was asked about.
    pub target: String,
    /// The repository source.
    pub source: String,
    /// The file, relative to the source root.
    pub path: String,
    /// The line range examined (1-based, inclusive); `None` for the whole file.
    pub lines: Option<(u32, u32)>,
    /// How the commits were found: `blame` (exact lines) or `history` (commits touching the file).
    pub method: String,
    /// The responsible commits, most lines first.
    pub commits: Vec<CommitTrail>,
    /// Pages and threads that discuss the file or symbol itself.
    pub references: Vec<Reference>,
    /// What could not be determined.
    pub gaps: Vec<String>,
}

fn attr(node: &NodeRecord, name: &str) -> Option<String> {
    node.attrs
        .get(name)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.len() <= SNIPPET {
        return text.to_owned();
    }
    let mut cut = SNIPPET;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

fn closed(node: &NodeRecord) -> Option<String> {
    let reason = attr(node, attrs::CLOSE_REASON)?;
    Some(match attr(node, attrs::CLOSED_BY) {
        Some(who) => format!("{reason} (by {who})"),
        None => reason,
    })
}

/// Splits `path:12` or `path:12-40` into the path and the line range.
fn split_lines(target: &str) -> (&str, Option<(u32, u32)>) {
    if let Some((path, range)) = target.rsplit_once(':') {
        let parsed = match range.split_once('-') {
            Some((start, end)) => start.parse().ok().zip(end.parse().ok()),
            None => range.parse().ok().map(|line| (line, line)),
        };
        if let Some((start, end)) = parsed
            && start >= 1
            && end >= start
            && !path.is_empty()
        {
            return (path, Some((start, end)));
        }
    }
    (target, None)
}

struct Located {
    source: SourceId,
    root: Option<PathBuf>,
    path: String,
    lines: Option<(u32, u32)>,
    node: Option<NodeId>,
}

impl Engine {
    fn repo_roots(&self) -> Vec<(SourceId, PathBuf)> {
        self.config()
            .sources
            .iter()
            .filter(|source| source.enabled && source.kind == SourceKind::Repo)
            .map(|source| (source.id.clone(), PathBuf::from(&source.location)))
            .collect()
    }

    /// Finds what `target` names: a path on disk, a path relative to a repository source
    /// (optionally `source:path`), or a symbol.
    fn locate_target(&self, target: &str) -> Result<Located> {
        let store = self.store();
        let (text, lines) = split_lines(target.trim());
        let roots = self.repo_roots();

        // A path on disk, absolute or relative to the current directory.
        let on_disk = std::path::absolute(Path::new(text))
            .ok()
            .filter(|p| p.is_file());
        if let Some(path) = on_disk {
            let canonical = path.canonicalize().unwrap_or(path);
            let found = roots
                .iter()
                .filter_map(|(id, root)| {
                    let root = root.canonicalize().ok()?;
                    let rel = canonical.strip_prefix(&root).ok()?;
                    Some((
                        id,
                        root.clone(),
                        keys::normalize_path(&rel.to_string_lossy()),
                    ))
                })
                .max_by_key(|(_, root, _)| root.as_os_str().len());
            if let Some((id, root, rel)) = found {
                let node = store.node_by_key(&keys::file(id, &rel))?.map(|n| n.id);
                return Ok(Located {
                    source: id.clone(),
                    root: Some(root),
                    path: rel,
                    lines,
                    node,
                });
            }
        }

        // `source:path`, or a path relative to one of the repositories.
        let (scope, rel) = match text.split_once(':') {
            Some((source, rest)) if roots.iter().any(|(id, _)| id.as_str() == source) => {
                (Some(source), rest)
            }
            _ => (None, text),
        };
        let rel = keys::normalize_path(rel);
        let mut matches: Vec<(SourceId, Option<PathBuf>)> = roots
            .iter()
            .filter(|(id, _)| scope.is_none_or(|scope| id.as_str() == scope))
            .filter(|(_, root)| root.join(&rel).is_file())
            .map(|(id, root)| (id.clone(), Some(root.clone())))
            .collect();
        if matches.is_empty() && (rel.contains('/') || rel.contains('.')) {
            // Not on this disk (a synced source): any file node with that path.
            for (id, _) in &roots {
                if store.node_by_key(&keys::file(id, &rel))?.is_some() {
                    matches.push((id.clone(), None));
                }
            }
            if matches.is_empty() {
                for source in store.sources()? {
                    if scope.is_none_or(|scope| source.id.as_str() == scope)
                        && store.node_by_key(&keys::file(&source.id, &rel))?.is_some()
                    {
                        matches.push((source.id, None));
                    }
                }
            }
        }
        match matches.len() {
            1 => {
                let (source, root) = matches.remove(0);
                let node = store.node_by_key(&keys::file(&source, &rel))?.map(|n| n.id);
                return Ok(Located {
                    source,
                    root,
                    path: rel,
                    lines,
                    node,
                });
            }
            0 => {}
            _ => {
                let names: Vec<String> = matches
                    .iter()
                    .map(|(id, _)| format!("{id}:{rel}"))
                    .collect();
                return Err(Error::invalid(format!(
                    "`{rel}` exists in several repositories; ask about one of: {}",
                    names.join(", ")
                )));
            }
        }

        // A symbol: its file and line range come from the graph.
        let symbol = match store.resolve(&Lookup::new(&[NodeKind::Symbol], text))? {
            Resolution::Resolved(node) => Some(node),
            Resolution::Ambiguous(nodes) => {
                let mut names = Vec::new();
                for node in store.nodes(&nodes)?.iter().take(6) {
                    names.push(node.key.to_string());
                }
                return Err(Error::invalid(format!(
                    "several symbols are called `{text}`: {}. Ask with `path:line` instead.",
                    names.join(", ")
                )));
            }
            Resolution::NotFound => store
                .search_nodes(text, &[NodeKind::Symbol], 1)?
                .into_iter()
                .find(|hit| hit.score >= 0.7)
                .map(|hit| hit.node),
        };
        let Some(node) = symbol.map(|id| store.node(id)).transpose()?.flatten() else {
            return Err(Error::not_found(format!(
                "`{text}` is not a file in a configured repository nor a known symbol"
            )));
        };
        let path = attr(&node, attrs::PATH)
            .ok_or_else(|| Error::not_found(format!("symbol `{text}` has no recorded file")))?;
        let number = |name: &str| {
            node.attrs
                .get(name)
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
        };
        let source = node
            .sources
            .first()
            .cloned()
            .ok_or_else(|| Error::not_found(format!("symbol `{text}` has no source")))?;
        let root = roots
            .iter()
            .find(|(id, _)| *id == source)
            .map(|(_, root)| root.clone())
            .filter(|root| root.join(&path).is_file());
        Ok(Located {
            source,
            root,
            path,
            lines: lines.or_else(|| number(attrs::LINE_START).zip(number(attrs::LINE_END))),
            node: Some(node.id),
        })
    }

    fn other(&self, edge: &EdgeRecord, this: NodeId) -> Result<Option<NodeRecord>> {
        self.store().node(if edge.from == this {
            edge.to
        } else {
            edge.from
        })
    }

    /// What a thread says: its own prose, else its first comment. The quoted diff hunk a review
    /// thread is anchored to is not what was said.
    fn thread_text(&self, thread: NodeId) -> Result<Option<String>> {
        let store = self.store();
        let prose = |node: NodeId| -> Result<Option<String>> {
            Ok(store
                .chunks_of(node)?
                .into_iter()
                .find(|chunk| chunk.kind != crate::base::ChunkKind::Code)
                .map(|chunk| clip(&chunk.text)))
        };
        if let Some(text) = prose(thread)? {
            return Ok(Some(text));
        }
        let mut comments: Vec<NodeId> = store
            .edges_of(thread, 100)?
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::Contains && edge.from == thread)
            .map(|edge| edge.to)
            .collect();
        comments.sort_unstable();
        for comment in comments {
            if let Some(text) = prose(comment)? {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }

    fn first_text(&self, node: NodeId) -> Result<Option<String>> {
        Ok(self
            .store()
            .chunks_of(node)?
            .into_iter()
            .next()
            .map(|chunk| clip(&chunk.text)))
    }

    /// Pages and threads that mention, document or discuss `node`.
    fn references_to(&self, node: &NodeRecord, about: &str) -> Result<Vec<Reference>> {
        let mut out = Vec::new();
        for edge in self.store().edges_of(node.id, 400)? {
            if edge.to != node.id
                || !matches!(
                    edge.kind,
                    EdgeKind::Mentions | EdgeKind::Documents | EdgeKind::Discusses
                )
            {
                continue;
            }
            let Some(other) = self.store().node(edge.from)? else {
                continue;
            };
            if !matches!(other.kind, NodeKind::WikiPage | NodeKind::Thread) {
                continue;
            }
            out.push(Reference {
                key: other.key.to_string(),
                kind: other.kind,
                title: other.name.clone(),
                about: about.to_owned(),
                trust: other.trust,
                url: attr(&other, attrs::URL),
                closed: closed(&other),
            });
        }
        // Decisions (closed threads) first, then authoritative sources.
        out.sort_by_key(|r| (r.closed.is_none(), r.trust != TrustLevel::SourceOfTruth));
        out.truncate(MAX_ITEMS);
        Ok(out)
    }

    fn ticket_trail(&self, node: &NodeRecord) -> Result<TicketTrail> {
        let mut blocked_by = Vec::new();
        for edge in self.store().edges_of(node.id, 200)? {
            if edge.kind != EdgeKind::Blocks || edge.to != node.id {
                continue;
            }
            let Some(blocker) = self.store().node(edge.from)? else {
                continue;
            };
            let reason = edge
                .claims
                .iter()
                .find_map(|claim| claim.attrs.get(attrs::REASON).and_then(|v| v.as_str()))
                .map(str::to_owned)
                .or_else(|| attr(&blocker, attrs::REASON));
            blocked_by.push(match reason {
                Some(reason) => format!("{}: {reason}", blocker.name),
                None => blocker.name.clone(),
            });
        }
        if let Some(reason) = attr(node, attrs::REASON) {
            blocked_by.push(reason);
        }
        Ok(TicketTrail {
            key: node.key.to_string(),
            title: attr(node, attrs::TITLE).unwrap_or_else(|| node.name.clone()),
            status: attr(node, attrs::STATUS),
            url: attr(node, attrs::URL),
            description: self.first_text(node.id)?,
            blocked_by,
        })
    }

    /// The pull request with its decisions and threads, and the tickets it tracks.
    fn pull_request_trail(&self, node: &NodeRecord) -> Result<(PullRequestTrail, Vec<NodeRecord>)> {
        let mut decisions = Vec::new();
        let mut discussions = Vec::new();
        let mut tickets = Vec::new();
        for edge in self.store().edges_of(node.id, 400)? {
            let Some(other) = self.other(&edge, node.id)? else {
                continue;
            };
            match (edge.kind, other.kind, edge.to == node.id) {
                (EdgeKind::Approves, NodeKind::Approval, true) => decisions.push(Decision {
                    title: other.name.clone(),
                    decision: attr(&other, attrs::DECISION),
                    reason: attr(&other, attrs::REASON).map(|reason| clip(&reason)),
                    closed: closed(&other),
                }),
                (EdgeKind::Contains, NodeKind::Thread, false) => discussions.push(Discussion {
                    title: other.name.clone(),
                    status: attr(&other, attrs::THREAD_STATUS),
                    anchor: attr(&other, attrs::ANCHOR_TEXT),
                    closed: closed(&other),
                    text: self.thread_text(other.id)?,
                }),
                (EdgeKind::Tracks | EdgeKind::Mentions, NodeKind::Ticket, _) => {
                    tickets.push(other);
                }
                _ => {}
            }
        }
        // What was dismissed or is still contested matters most.
        discussions.sort_by_key(|d| match d.status.as_deref() {
            Some(attrs::THREAD_DISMISSED) => 0,
            Some(attrs::THREAD_OPEN) => 1,
            _ => 2,
        });
        discussions.truncate(MAX_ITEMS);
        // Decisions that were overturned or that asked for changes say the most about why the
        // code ended up as it is; automated reviewers say the least.
        decisions.sort_by_key(|d| {
            let weight = match (d.closed.is_some(), d.decision.as_deref()) {
                (true, _) => 0,
                (_, Some(attrs::DECISION_CHANGES_REQUESTED | attrs::DECISION_REJECTED)) => 1,
                (_, Some(attrs::DECISION_APPROVED)) => 2,
                _ => 3,
            };
            (d.title.contains("[bot]"), weight)
        });
        decisions.truncate(MAX_ITEMS);
        Ok((
            PullRequestTrail {
                key: node.key.to_string(),
                title: attr(node, attrs::TITLE).unwrap_or_else(|| node.name.clone()),
                status: attr(node, attrs::STATUS),
                url: attr(node, attrs::URL),
                description: self.first_text(node.id)?,
                decisions,
                discussions,
            },
            tickets,
        ))
    }

    fn commit_trail(
        &self,
        source: &SourceId,
        commit: &CommitInfo,
        lines: u32,
        repo_names: &[String],
    ) -> Result<CommitTrail> {
        let store = self.store();
        let refs = history::refs(&commit.message);
        let mut gaps = Vec::new();

        // Pull requests: named in the message, or linked to the commit node in the graph.
        let mut pulls: Vec<NodeRecord> = Vec::new();
        let mut seen = BTreeSet::new();
        for number in &refs.pulls {
            let found = repo_names
                .iter()
                .find_map(|repo| {
                    store
                        .node_by_key(&keys::pull_request(repo, *number))
                        .transpose()
                })
                .transpose()?;
            match found {
                Some(node) => {
                    if seen.insert(node.id) {
                        pulls.push(node);
                    }
                }
                None => gaps.push(format!(
                    "the message refers to #{number}, but no pull request with that number is ingested (it may be an issue, or its review was not fetched; try `anchor fetch`)"
                )),
            }
        }
        let mut ticket_nodes: Vec<NodeRecord> = Vec::new();
        if let Some(node) = store.node_by_key(&keys::commit(source, &commit.sha))? {
            for edge in store.edges_of(node.id, 200)? {
                if edge.to != node.id {
                    continue;
                }
                let Some(other) = store.node(edge.from)? else {
                    continue;
                };
                match (edge.kind, other.kind) {
                    (EdgeKind::Contains, NodeKind::PullRequest) => {
                        if seen.insert(other.id) {
                            pulls.push(other);
                        }
                    }
                    (EdgeKind::Tracks, NodeKind::Ticket) => ticket_nodes.push(other),
                    _ => {}
                }
            }
        }
        for key in &refs.tickets {
            match store.node_by_key(&keys::ticket(key))? {
                Some(node) => ticket_nodes.push(node),
                None => gaps.push(format!(
                    "the message names {key}, but that ticket is not ingested"
                )),
            }
        }

        let mut pull_requests = Vec::new();
        for pull in &pulls {
            let (trail, tracked) = self.pull_request_trail(pull)?;
            ticket_nodes.extend(tracked);
            pull_requests.push(trail);
        }
        let mut tickets = Vec::new();
        let mut references = Vec::new();
        let mut seen_tickets = BTreeSet::new();
        for node in &ticket_nodes {
            if seen_tickets.insert(node.id) {
                tickets.push(self.ticket_trail(node)?);
                references.extend(self.references_to(node, &node.name)?);
            }
        }
        for pull in &pulls {
            references.extend(self.references_to(pull, &pull.name)?);
        }
        let mut seen_refs = BTreeSet::new();
        references.retain(|r| seen_refs.insert(r.key.clone()));
        references.truncate(MAX_ITEMS);

        if pull_requests.is_empty() && refs.pulls.is_empty() {
            gaps.push("no pull request is linked to this commit: it may have been pushed directly, or the review is not ingested".into());
        }
        if tickets.is_empty() && refs.tickets.is_empty() {
            gaps.push("no ticket is named by the commit or its pull request".into());
        }
        let mut message = commit.message.splitn(2, '\n');
        let subject = message.next().unwrap_or("").trim().to_owned();
        Ok(CommitTrail {
            sha: commit.sha.clone(),
            author: commit.author.clone(),
            date: commit.date.clone(),
            subject,
            change_type: history::change_type(&commit.message).map(str::to_owned),
            body: clip(message.next().unwrap_or("")),
            lines,
            pull_requests,
            tickets,
            references,
            gaps,
        })
    }

    /// Commits known to the graph that modified the file, newest first (for sources whose
    /// working tree is not on this machine).
    fn commits_from_graph(&self, file: NodeId) -> Result<Vec<CommitInfo>> {
        let store = self.store();
        let mut commits = Vec::new();
        for edge in store.edges_of(file, 500)? {
            if edge.kind != EdgeKind::Modifies || edge.to != file {
                continue;
            }
            let Some(node) = store.node(edge.from)? else {
                continue;
            };
            let message = store
                .chunks_of(node.id)?
                .into_iter()
                .next()
                .map_or_else(|| node.name.clone(), |chunk| chunk.text);
            let mut author = String::new();
            for edge in store.edges_of(node.id, 50)? {
                if edge.kind == EdgeKind::AuthoredBy
                    && edge.from == node.id
                    && let Some(person) = store.node(edge.to)?
                {
                    author = person.name;
                }
            }
            commits.push(CommitInfo {
                sha: attr(&node, history::ATTR_SHA).unwrap_or_default(),
                author,
                email: String::new(),
                date: attr(&node, attrs::CREATED_AT).unwrap_or_default(),
                message,
                files: Vec::new(),
            });
        }
        commits.sort_by(|a, b| b.date.cmp(&a.date));
        commits.truncate(MAX_COMMITS);
        Ok(commits)
    }

    /// Explains why a file, a range of lines or a symbol is the way it is. Blocking: runs git.
    pub fn why(&self, target: &str) -> Result<WhyReport> {
        let located = self.locate_target(target)?;
        let mut gaps = Vec::new();
        let mut method = "blame";
        let mut selected: Vec<(CommitInfo, u32)> = Vec::new();
        let git_root = located
            .root
            .as_deref()
            .filter(|root| history::is_work_tree(root));
        let mut repo_names = vec![keys::slugify(located.source.as_str())];
        if let Some(root) = git_root {
            repo_names = history::repo_names(root, &located.source);
            match history::blame(root, &located.path, located.lines) {
                Ok(blamed) => {
                    if blamed.iter().any(history::Blamed::uncommitted) {
                        gaps.push("some of these lines are not committed yet".into());
                    }
                    let committed: Vec<_> = blamed
                        .into_iter()
                        .filter(|b| !b.uncommitted())
                        .take(MAX_COMMITS)
                        .collect();
                    let shas: Vec<String> = committed.iter().map(|b| b.sha.clone()).collect();
                    let details = history::show(root, &shas)?;
                    for blamed in committed {
                        if let Some(info) = details.iter().find(|c| c.sha == blamed.sha) {
                            selected.push((info.clone(), blamed.lines));
                        }
                    }
                }
                Err(err) => {
                    // Untracked file, or a range beyond the end of the file.
                    gaps.push(format!("blame is not available: {err}"));
                    method = "history";
                    for info in
                        history::log_path(root, &located.path, MAX_COMMITS).unwrap_or_default()
                    {
                        selected.push((info, 0));
                    }
                }
            }
        } else {
            method = "history";
            gaps.push(
                "the repository's working tree is not on this machine (or is not a git repository): using the ingested history, which is per file, not per line"
                    .into(),
            );
            if let Some(file) = located.node {
                for info in self.commits_from_graph(file)? {
                    selected.push((info, 0));
                }
            }
        }
        if selected.is_empty() {
            gaps.push("no commit was found for this target".into());
        }

        let mut commits = Vec::new();
        for (info, lines) in &selected {
            commits.push(self.commit_trail(&located.source, info, *lines, &repo_names)?);
        }
        let references = match located
            .node
            .map(|id| self.store().node(id))
            .transpose()?
            .flatten()
        {
            Some(node) => self.references_to(&node, &node.name)?,
            None => Vec::new(),
        };
        Ok(WhyReport {
            target: target.trim().to_owned(),
            source: located.source.to_string(),
            path: located.path,
            lines: located.lines,
            method: method.to_owned(),
            commits,
            references,
            gaps,
        })
    }
}

fn push_reference(out: &mut String, indent: &str, reference: &Reference) {
    out.push_str(&format!(
        "{indent}- {} \"{}\" [{}] discusses {}",
        reference.kind, reference.title, reference.trust, reference.about
    ));
    if let Some(closed) = &reference.closed {
        out.push_str(&format!(" — closed: {closed}"));
    }
    if let Some(url) = &reference.url {
        out.push_str(&format!(" <{url}>"));
    }
    out.push('\n');
}

/// Renders the report for a terminal or for an agent to read.
pub fn render(report: &WhyReport) -> String {
    let mut out = format!("{}:{}", report.source, report.path);
    if let Some((start, end)) = report.lines {
        out.push_str(&format!(" lines {start}-{end}"));
    }
    out.push_str(&format!(" (by {})\n", report.method));
    for commit in &report.commits {
        out.push_str(&format!(
            "\ncommit {} by {} on {}",
            &commit.sha[..commit.sha.len().min(10)],
            commit.author,
            commit.date.split('T').next().unwrap_or(&commit.date)
        ));
        if commit.lines > 0 {
            out.push_str(&format!(" — {} line(s)", commit.lines));
        }
        if let Some(kind) = &commit.change_type {
            out.push_str(&format!(" [{kind}]"));
        }
        out.push_str(&format!("\n  {}\n", commit.subject));
        if !commit.body.is_empty() {
            for line in commit.body.lines().take(8) {
                out.push_str(&format!("  | {line}\n"));
            }
        }
        for pull in &commit.pull_requests {
            out.push_str(&format!(
                "  pull request {} \"{}\"{}\n",
                pull.key,
                pull.title,
                pull.status
                    .as_deref()
                    .map(|s| format!(" [{s}]"))
                    .unwrap_or_default()
            ));
            if let Some(description) = &pull.description {
                out.push_str(&format!("    {}\n", description.replace('\n', " ")));
            }
            for decision in &pull.decisions {
                out.push_str(&format!("    - {}", decision.title));
                if let Some(reason) = &decision.reason {
                    out.push_str(&format!(": {reason}"));
                }
                if let Some(closed) = &decision.closed {
                    out.push_str(&format!(" — dismissed: {closed}"));
                }
                out.push('\n');
            }
            for discussion in &pull.discussions {
                out.push_str(&format!(
                    "    - thread [{}]",
                    discussion.status.as_deref().unwrap_or("?")
                ));
                if let Some(anchor) = &discussion.anchor {
                    out.push_str(&format!(" on \"{}\"", anchor.replace('\n', " ")));
                }
                if let Some(text) = &discussion.text {
                    out.push_str(&format!(": {}", text.replace('\n', " ")));
                }
                if let Some(closed) = &discussion.closed {
                    out.push_str(&format!(" — closed: {closed}"));
                }
                out.push('\n');
            }
        }
        for ticket in &commit.tickets {
            out.push_str(&format!(
                "  ticket {} \"{}\"{}\n",
                ticket.key.trim_start_matches("ticket:"),
                ticket.title,
                ticket
                    .status
                    .as_deref()
                    .map(|s| format!(" [{s}]"))
                    .unwrap_or_default()
            ));
            if let Some(description) = &ticket.description {
                out.push_str(&format!("    {}\n", description.replace('\n', " ")));
            }
            for blocker in &ticket.blocked_by {
                out.push_str(&format!("    - blocked by {blocker}\n"));
            }
        }
        for reference in &commit.references {
            push_reference(&mut out, "  ", reference);
        }
        for gap in &commit.gaps {
            out.push_str(&format!("  trail ends: {gap}\n"));
        }
    }
    if !report.references.is_empty() {
        out.push_str("\nalso discussed in\n");
        for reference in &report.references {
            push_reference(&mut out, "  ", reference);
        }
    }
    for gap in &report.gaps {
        out.push_str(&format!("\nnote: {gap}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_are_split_from_the_target() {
        assert_eq!(split_lines("src/a.rs:12"), ("src/a.rs", Some((12, 12))));
        assert_eq!(split_lines("src/a.rs:12-40"), ("src/a.rs", Some((12, 40))));
        assert_eq!(split_lines("ledger:src/a.rs"), ("ledger:src/a.rs", None));
        assert_eq!(
            split_lines("ledger:src/a.rs:7"),
            ("ledger:src/a.rs", Some((7, 7)))
        );
        assert_eq!(split_lines("charge_card"), ("charge_card", None));
        assert_eq!(split_lines("src/a.rs:40-12"), ("src/a.rs:40-12", None));
        assert_eq!(split_lines("Ledger::charge"), ("Ledger::charge", None));
    }
}
