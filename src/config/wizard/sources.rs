//! The sources step: adding, discovering, editing, re-trusting and removing sources, each with an
//! explicitly chosen trust level.

use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::base::{SourceId, SourceKind, TrustLevel};
use crate::config::{SourceConfig, is_valid_source_id};

use super::Choice;
use super::flow::{Session, Step};

/// Option keys of a wiki source (read by the wiki ingester).
pub const OPT_SCRAPE_COMMENTS: &str = "scrape_comments";
/// Whether dismissed and resolved comment threads are ingested too.
pub const OPT_INCLUDE_DISMISSED: &str = "include_dismissed_threads";

const KINDS: [(SourceKind, &str, &str); 6] = [
    (
        SourceKind::Repo,
        "Repository",
        "a source code checkout on disk",
    ),
    (
        SourceKind::Wiki,
        "Wiki snapshot",
        "pages with comment threads",
    ),
    (
        SourceKind::ApiPortal,
        "API approval portal",
        "API definitions plus approval records",
    ),
    (SourceKind::Jira, "Jira export", "issue JSON payloads"),
    (
        SourceKind::OrgModel,
        "Org / portfolio model",
        "portfolios, domains, components, teams",
    ),
    (
        SourceKind::CodeReview,
        "Code-review export",
        "pull requests with review threads",
    ),
];

/// Deepest directory level scanned when discovering repositories.
const DISCOVERY_DEPTH: usize = 5;
/// Most repositories discovery reports.
const DISCOVERY_LIMIT: usize = 200;

fn kind_label(kind: SourceKind) -> &'static str {
    KINDS
        .iter()
        .find(|(k, ..)| *k == kind)
        .map_or("Source", |(_, label, _)| label)
}

/// A well-formed source id derived from a human name: lowercase letters, digits, `.`, `_`, `-`;
/// runs of anything else become one `-`. `None` when nothing usable remains.
pub fn slugify(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(64)
        .collect();
    (is_valid_source_id(&slug)).then_some(slug)
}

/// Finds git repositories below `root` (a directory holding `.git`), not descending into
/// repositories, `node_modules` or `target`.
pub fn discover_repositories(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut walk = WalkDir::new(root)
        .max_depth(DISCOVERY_DEPTH)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walk.next() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == "node_modules" || name == "target" || name == ".git" {
            walk.skip_current_dir();
            continue;
        }
        if entry.path().join(".git").exists() {
            found.push(entry.path().to_path_buf());
            walk.skip_current_dir();
            if found.len() >= DISCOVERY_LIMIT {
                break;
            }
        }
    }
    found.sort();
    found
}

enum Action {
    Add,
    Discover,
    Edit,
    Remove,
    Done,
}

impl Session<'_> {
    fn trust_choices() -> [Choice; 2] {
        [
            Choice::with_help(
                "SourceOfTruth",
                "authoritative baseline: its facts win conflicts and are traversed first",
            ),
            Choice::with_help(
                "ReferenceOnly",
                "contextual fallback: explains and fills gaps, never overrides a source of truth",
            ),
        ]
    }

    /// Asks for the trust level; the default for `kind` (or the current level) is preselected.
    fn ask_trust(
        &self,
        subject: &str,
        kind: SourceKind,
        current: Option<TrustLevel>,
    ) -> Step<TrustLevel> {
        let recommended = current.unwrap_or_else(|| SourceConfig::default_trust(kind));
        let default = usize::from(recommended != TrustLevel::SourceOfTruth);
        let message = format!(
            "Trust level for {subject} ({}); recommended: {}",
            kind_label(kind),
            if recommended == TrustLevel::SourceOfTruth {
                "SourceOfTruth"
            } else {
                "ReferenceOnly"
            }
        );
        let picked = self.p.select(&message, &Self::trust_choices(), default)?;
        Ok(if picked == 0 {
            TrustLevel::SourceOfTruth
        } else {
            TrustLevel::ReferenceOnly
        })
    }

    fn unique_id(&self, base: &str) -> String {
        let taken = |id: &str| self.config.sources.iter().any(|s| s.id.as_str() == id);
        if !taken(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|candidate| !taken(candidate))
            .unwrap_or_else(|| base.to_owned())
    }

    fn expand_path(&self, text: &str) -> String {
        if text.contains("://") {
            return text.to_owned();
        }
        let expanded = match text.strip_prefix("~/") {
            Some(rest) => match self.env.lookup("HOME") {
                Some(home) => PathBuf::from(home).join(rest),
                None => PathBuf::from(text),
            },
            None => PathBuf::from(text),
        };
        std::path::absolute(&expanded)
            .unwrap_or(expanded)
            .to_string_lossy()
            .into_owned()
    }

    fn ask_location(&self, kind: SourceKind, default: Option<&str>) -> Step<String> {
        let message = match kind {
            SourceKind::Repo => "Path of the repository",
            _ => "Path (or URL) of the export / snapshot",
        };
        let text = self.ask_valid(message, default, |text| {
            if text.is_empty() {
                Err("a location is required".to_owned())
            } else {
                Ok(text.to_owned())
            }
        })?;
        let location = self.expand_path(&text);
        if !location.contains("://") && !Path::new(&location).exists() {
            self.p.warn(&format!(
                "{location} does not exist yet; the source stays empty until it does."
            ));
        }
        Ok(location)
    }

    fn ask_wiki_options(&self, source: &mut SourceConfig) -> Step {
        let current = |key: &str, default: bool| {
            source
                .options
                .get(key)
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        let scrape_default = current(OPT_SCRAPE_COMMENTS, true);
        let dismissed_default = current(OPT_INCLUDE_DISMISSED, true);
        let scrape = self.p.confirm(
            "Scrape comment threads from this wiki (discussions often hold the real decisions)?",
            scrape_default,
        )?;
        let dismissed = scrape
            && self.p.confirm(
                "Include dismissed and resolved threads (rejected ideas explain why things are the way they are)?",
                dismissed_default,
            )?;
        source
            .options
            .insert(OPT_SCRAPE_COMMENTS.to_owned(), Value::Bool(scrape));
        source
            .options
            .insert(OPT_INCLUDE_DISMISSED.to_owned(), Value::Bool(dismissed));
        Ok(())
    }

    fn ask_kind(&self) -> Step<SourceKind> {
        let choices: Vec<Choice> = KINDS
            .iter()
            .map(|(_, label, help)| Choice::with_help(*label, *help))
            .collect();
        Ok(KINDS[self.p.select("What kind of source?", &choices, 0)?].0)
    }

    fn add_source(&mut self) -> Step {
        let kind = self.ask_kind()?;
        let location = self.ask_location(kind, None)?;
        let suggestion = Path::new(&location)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|_| !location.contains("://"))
            .unwrap_or_else(|| kind.as_str().to_owned());
        let name = self.ask_valid("Name of the source (used to derive its id)", Some(&suggestion), |text| {
            slugify(text)
                .map(|_| text.to_owned())
                .ok_or_else(|| {
                    format!("`{text}` cannot become a source id: use letters or digits (ids are lowercase letters, digits, `.`, `_`, `-`)")
                })
        })?;
        let base = slugify(&name).unwrap_or_else(|| kind.as_str().to_owned());
        let id = self.unique_id(&base);
        if id != base {
            self.p
                .note(&format!("The id `{base}` is taken; using `{id}`."));
        }
        let trust = self.ask_trust(&format!("`{id}`"), kind, None)?;
        let mut source = SourceConfig::new(SourceId::new(id), kind, location);
        source.trust = trust;
        if name != source.id.as_str() {
            source.name = Some(name);
        }
        if kind == SourceKind::Wiki {
            self.ask_wiki_options(&mut source)?;
        }
        self.config.sources.push(source);
        Ok(())
    }

    async fn discover(&mut self) -> Step {
        let root = self
            .p
            .path("Directory to search for git repositories", None)?;
        let root = PathBuf::from(self.expand_path(&root.to_string_lossy()));
        let scan = root.clone();
        let found = tokio::task::spawn_blocking(move || discover_repositories(&scan))
            .await
            .unwrap_or_default();
        let known: Vec<&str> = self
            .config
            .sources
            .iter()
            .map(|s| s.location.as_str())
            .collect();
        let fresh: Vec<PathBuf> = found
            .into_iter()
            .filter(|path| !known.contains(&path.to_string_lossy().as_ref()))
            .collect();
        if fresh.is_empty() {
            self.p.warn(&format!(
                "No new git repositories found under {}.",
                root.display()
            ));
            return Ok(());
        }
        let choices: Vec<Choice> = fresh
            .iter()
            .map(|path| {
                Choice::new(
                    path.strip_prefix(&root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .collect();
        let defaults: Vec<usize> = (0..fresh.len()).collect();
        let picked = self.p.multi_select(
            &format!("Repositories under {} to add", root.display()),
            &choices,
            &defaults,
        )?;
        if picked.is_empty() {
            return Ok(());
        }
        let trust = self.ask_trust(
            &format!("the {} selected repositories", picked.len()),
            SourceKind::Repo,
            None,
        )?;
        for index in picked {
            let path = &fresh[index];
            let base = path
                .file_name()
                .and_then(|n| slugify(&n.to_string_lossy()))
                .unwrap_or_else(|| "repo".to_owned());
            let id = self.unique_id(&base);
            let mut source = SourceConfig::new(
                SourceId::new(id),
                SourceKind::Repo,
                path.to_string_lossy().into_owned(),
            );
            source.trust = trust;
            self.config.sources.push(source);
        }
        Ok(())
    }

    fn pick_source(&self, message: &str) -> Step<usize> {
        let choices: Vec<Choice> = self
            .config
            .sources
            .iter()
            .map(|s| Choice::with_help(s.id.as_str(), format!("{}, {:?}", s.kind, s.trust)))
            .collect();
        Ok(self.p.select(message, &choices, 0)?)
    }

    fn edit_source(&mut self) -> Step {
        let index = self.pick_source("Which source?")?;
        let source = self.config.sources[index].clone();
        let mut actions = vec![("Change the trust level", 0), ("Change the location", 1)];
        if source.kind == SourceKind::Wiki {
            actions.push(("Wiki comment options", 2));
        }
        actions.push(("Back", 3));
        let choices: Vec<Choice> = actions.iter().map(|(l, _)| Choice::new(*l)).collect();
        let picked = actions[self
            .p
            .select(&format!("Edit `{}`", source.id), &choices, 0)?]
        .1;
        let mut edited = source.clone();
        match picked {
            0 => {
                edited.trust =
                    self.ask_trust(&format!("`{}`", source.id), source.kind, Some(source.trust))?
            }
            1 => edited.location = self.ask_location(source.kind, Some(&source.location))?,
            2 => self.ask_wiki_options(&mut edited)?,
            _ => return Ok(()),
        }
        self.config.sources[index] = edited;
        Ok(())
    }

    fn remove_source(&mut self) -> Step {
        let index = self.pick_source("Which source to remove?")?;
        let id = self.config.sources[index].id.clone();
        if self
            .p
            .confirm(&format!("Remove `{id}` from the configuration?"), false)?
        {
            self.config.sources.remove(index);
            self.config.sync.publish.retain(|p| *p != id);
            self.config.sync.subscribe.retain(|p| *p != id);
        }
        Ok(())
    }

    pub(super) async fn step_sources(&mut self) -> Step {
        for preset in self.opts.sources.clone() {
            self.config.sources.retain(|s| s.id != preset.id);
            self.config.sources.push(preset);
        }
        loop {
            if self.config.sources.is_empty() {
                self.p.note("No sources are configured yet.");
            } else {
                self.p.note("Sources:");
                for s in &self.config.sources {
                    self.p.note(&format!(
                        "  {} [{}] {:?} {}",
                        s.id, s.kind, s.trust, s.location
                    ));
                }
            }
            let mut menu = vec![
                (Action::Add, Choice::new("Add a source")),
                (
                    Action::Discover,
                    Choice::new("Discover git repositories under a directory"),
                ),
            ];
            if !self.config.sources.is_empty() {
                menu.push((Action::Edit, Choice::new("Edit or re-trust a source")));
                menu.push((Action::Remove, Choice::new("Remove a source")));
            }
            menu.push((Action::Done, Choice::new("Done with sources")));
            let done = menu.len() - 1;
            let default = if self.config.sources.is_empty() && !self.opts.yes {
                0
            } else {
                done
            };
            let choices: Vec<Choice> = menu.iter().map(|(_, c)| c.clone()).collect();
            let picked = self
                .p
                .select("Sources and trust levels", &choices, default)?;
            match menu[picked].0 {
                Action::Add => self.add_source()?,
                Action::Discover => self.discover().await?,
                Action::Edit => self.edit_source()?,
                Action::Remove => self.remove_source()?,
                Action::Done => return Ok(()),
            }
        }
    }
}
