//! An optional git history for the generated repositories, so that `anchor why` has commits to
//! blame on sample data.
//!
//! Each repository gets an initial import, then one commit per merged pull request of the plan:
//! it appends a line to the service's main source file and carries the message a squash merge
//! would, `<TICKET>: <title> (#<number>)`, authored by the pull request's author on its merge
//! day. `anchor why` then follows each blamed line to the pull request, its review decisions and
//! its ticket. Kept out of [`super::generate`] so the corpus the tests score against is unchanged.

use std::path::Path;
use std::process::Command;

use super::code;
use super::plan::{Plan, ReviewScenario, timestamp};
use super::spec::{SynthLanguage, SynthSpec};
use crate::base::{Error, Result};

/// What [`init_git`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitHistory {
    /// Repositories turned into git repositories.
    pub repositories: usize,
    /// Commits made, initial imports included.
    pub commits: usize,
    /// A `source:path:line` whose blame leads to a pull request with a dismissed review, when
    /// the plan has one: a good first `anchor why`.
    pub example: Option<String>,
}

fn git(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .output()
        .map_err(|err| Error::other(format!("cannot run git: {err}")))?;
    if !output.status.success() {
        return Err(Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn comment(language: SynthLanguage) -> &'static str {
    match language {
        SynthLanguage::Python => "#",
        _ => "//",
    }
}

/// `true` when a `git` executable can be run.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Gives every generated repository under `root` a git history (see the module documentation).
/// `spec` must be the one the tree was generated from.
///
/// # Errors
///
/// Fails when git cannot be run or a repository cannot be written.
pub fn init_git(spec: &SynthSpec, root: &Path) -> Result<GitHistory> {
    let plan = Plan::build(spec);
    let mut history = GitHistory::default();
    for (index, service) in plan.services.iter().enumerate() {
        let repo = root.join("repos").join(&service.slug);
        if !repo.is_dir() || repo.join(".git").exists() {
            continue;
        }
        let owner = &plan.people[index % plan.people.len().max(1)];
        let identity = |name: &str, email: &str, when: String| {
            vec![
                ("GIT_AUTHOR_NAME", name.to_owned()),
                ("GIT_AUTHOR_EMAIL", email.to_owned()),
                ("GIT_AUTHOR_DATE", when.clone()),
                ("GIT_COMMITTER_NAME", name.to_owned()),
                ("GIT_COMMITTER_EMAIL", email.to_owned()),
                ("GIT_COMMITTER_DATE", when),
            ]
        };
        git(&repo, &["init", "-q"], &[])?;
        git(&repo, &["add", "-A"], &[])?;
        git(
            &repo,
            &[
                "commit",
                "-q",
                "-m",
                &format!("Initial import of {}", service.slug),
            ],
            &identity(&owner.name, &owner.email, timestamp(0, 30_000)),
        )?;
        history.repositories += 1;
        history.commits += 1;

        let path = code::source_path(service, 0);
        let mut prs: Vec<_> = plan
            .pull_requests
            .iter()
            .filter(|pr| pr.service == index && pr.scenario != ReviewScenario::Blocking)
            .collect();
        prs.sort_by_key(|pr| (pr.day, pr.number));
        for pr in prs {
            let ticket = pr.ticket.map(|t| plan.tickets[t].key.clone());
            let file = repo.join(&path);
            let mut text = std::fs::read_to_string(&file)?;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!(
                "{} {}{}\n",
                comment(service.language),
                pr.title,
                ticket
                    .as_ref()
                    .map(|key| format!(" ({key})"))
                    .unwrap_or_default()
            ));
            std::fs::write(&file, &text)?;
            let line = text.lines().count();
            let subject = match &ticket {
                Some(key) => format!("{key}: {} (#{})", pr.title, pr.number),
                None => format!("{} (#{})", pr.title, pr.number),
            };
            let author = &plan.people[pr.author];
            git(&repo, &["add", "-A"], &[])?;
            git(
                &repo,
                &["commit", "-q", "-m", &subject],
                &identity(&author.name, &author.email, timestamp(pr.day + 3, 29_000)),
            )?;
            history.commits += 1;
            if history.example.is_none() && pr.scenario == ReviewScenario::Dismissed {
                history.example = Some(format!("{}:{path}:{line}", service.slug));
            }
        }
    }
    Ok(history)
}
