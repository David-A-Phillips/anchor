//! `anchor source`: list, add and remove configured sources without editing the file by hand.
//!
//! `add` infers what it can (kind from what the path holds, id from its name, trust from the
//! kind), writes the configuration and ingests the new source. `remove` takes the source out of
//! the configuration and its facts out of the graph.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Subcommand;

use crate::base::{Error, Result, SourceId, SourceKind, TrustLevel, keys};
use crate::config::{AnchorConfig, Depth, SourceConfig};
use crate::engine::Engine;

/// Subcommands of `anchor source`.
#[derive(Debug, Subcommand)]
pub enum SourceAction {
    /// Show the configured sources.
    List,
    /// Add a source and ingest it.
    Add {
        /// Directory (or organisation-model file) to add.
        location: PathBuf,
        /// Source id; defaults to the directory or file name.
        #[arg(long)]
        id: Option<String>,
        /// repo | wiki | api_portal | jira | org_model | code_review | issues; inferred when omitted.
        #[arg(long)]
        kind: Option<SourceKind>,
        /// source_of_truth | reference_only; the default for the kind when omitted.
        #[arg(long)]
        trust: Option<TrustLevel>,
        /// phone_book | map | full.
        #[arg(long, default_value = "full")]
        depth: Depth,
        /// Only write the configuration; do not ingest now.
        #[arg(long)]
        no_ingest: bool,
    },
    /// Remove a source from the configuration and its facts from the graph.
    Remove {
        /// Source id.
        id: String,
        /// Leave what was ingested from it in the graph.
        #[arg(long)]
        keep_data: bool,
    },
}

/// What a path most plausibly is: a single structured file is an organisation model, a
/// directory is a repository unless its contents say otherwise.
pub fn infer_kind(location: &Path) -> SourceKind {
    if location.is_file() {
        return SourceKind::OrgModel;
    }
    if location.join(".git").exists() {
        return SourceKind::Repo;
    }
    let names: Vec<String> = std::fs::read_dir(location)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_lowercase())
                .take(200)
                .collect()
        })
        .unwrap_or_default();
    let has = |suffix: &str| names.iter().any(|name| name.ends_with(suffix));
    const CODE: &[&str] = &[
        ".rs",
        ".py",
        ".go",
        ".ts",
        ".js",
        ".java",
        ".kt",
        ".c",
        ".h",
        ".cpp",
        ".cs",
        ".rb",
        ".php",
        ".swift",
        ".scala",
        "cargo.toml",
        "package.json",
        "go.mod",
        "pom.xml",
    ];
    let has_code = CODE.iter().any(|suffix| has(suffix));
    let pages = has(".md") || has(".markdown") || has(".html") || has(".htm");
    // A folder of pages with no version control and no code is an exported wiki.
    if has(".comments.json") || (pages && !has_code) {
        SourceKind::Wiki
    } else {
        SourceKind::Repo
    }
}

/// Builds the source entry for `location`, checking it against the existing configuration.
pub fn new_source(
    config: &AnchorConfig,
    location: &Path,
    id: Option<&str>,
    kind: Option<SourceKind>,
    trust: Option<TrustLevel>,
    depth: Depth,
) -> Result<SourceConfig> {
    let location = location
        .canonicalize()
        .map_err(|err| Error::invalid(format!("{}: {err}", location.display())))?;
    let id = match id {
        Some(id) => id.trim().to_owned(),
        None => keys::slugify(
            &location
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    };
    if id.is_empty() {
        return Err(Error::invalid("cannot derive a source id; pass --id"));
    }
    let location_text = location.to_string_lossy().into_owned();
    if let Some(existing) = config
        .sources
        .iter()
        .find(|source| source.id.as_str() == id || source.location == location_text)
    {
        return Err(Error::invalid(format!(
            "source `{}` already covers {} (pass --id for a second entry under another name)",
            existing.id, existing.location
        )));
    }
    let kind = kind.unwrap_or_else(|| infer_kind(&location));
    if kind == SourceKind::Derived {
        return Err(Error::invalid(
            "`derived` is reserved for Anchor's own facts",
        ));
    }
    let mut source = SourceConfig::new(id.as_str(), kind, location_text);
    if let Some(trust) = trust {
        source.trust = trust;
    }
    source.depth = depth;
    Ok(source)
}

fn open(config: AnchorConfig) -> Result<Arc<Engine>> {
    Engine::open(config)
}

/// Runs one `anchor source` subcommand against the configuration at `path`. Blocking.
pub fn run(action: SourceAction, mut config: AnchorConfig, path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    match action {
        SourceAction::List => {
            if config.sources.is_empty() {
                out.push("No sources configured. Add one with `anchor source add <path>`.".into());
            } else {
                out.push(format!(
                    "{:<24} {:<12} {:<16} {:<10} {}",
                    "ID", "KIND", "TRUST", "DEPTH", "LOCATION"
                ));
            }
            for source in &config.sources {
                out.push(format!(
                    "{:<24} {:<12} {:<16} {:<10} {}{}",
                    source.id,
                    source.kind,
                    source.trust,
                    source.depth.as_str(),
                    source.location,
                    if source.enabled { "" } else { "  (disabled)" }
                ));
            }
        }
        SourceAction::Add {
            location,
            id,
            kind,
            trust,
            depth,
            no_ingest,
        } => {
            let inferred = kind.is_none() || trust.is_none();
            let source = new_source(&config, &location, id.as_deref(), kind, trust, depth)?;
            let id = source.id.clone();
            out.push(format!(
                "Added `{id}` ({}, {}) at {}.",
                source.kind, source.trust, source.location
            ));
            if inferred {
                out.push(format!(
                    "  Kind and trust were inferred; to change them: `anchor source remove {id}`, then add it again with --kind (repo, wiki, jira, ...) and --trust (source_of_truth or reference_only)."
                ));
            }
            config.sources.push(source);
            config.save(path)?;
            if no_ingest {
                out.push(format!("Run `anchor ingest --source {id}` to ingest it."));
            } else {
                let engine = open(config)?;
                let report = engine.scan_source(&id)?;
                engine.flush()?;
                out.push(format!(
                    "Ingested {} file{}: {} nodes, {} edges in {} ms.",
                    report.files_seen,
                    if report.files_seen == 1 { "" } else { "s" },
                    report.nodes_upserted,
                    report.edges_upserted,
                    report.duration_ms
                ));
                for warning in report.warnings.iter().take(5) {
                    out.push(format!("warning: {warning}"));
                }
            }
        }
        SourceAction::Remove { id, keep_data } => {
            let source = SourceId::new(id.as_str());
            let before = config.sources.len();
            config.sources.retain(|entry| entry.id != source);
            if config.sources.len() == before {
                return Err(Error::not_found(format!("no source is called `{id}`")));
            }
            config.save(path)?;
            out.push(format!("Removed `{id}` from the configuration."));
            if !keep_data {
                let engine = open(config)?;
                let keys: Vec<_> = engine
                    .store()
                    .unit_metas(&source)?
                    .into_iter()
                    .filter(|meta| !meta.deleted)
                    .map(|meta| meta.key)
                    .collect();
                let count = keys.len();
                engine.remove(&keys)?;
                engine.flush()?;
                out.push(format!("Removed {count} ingested units from the graph."));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn config(dir: &Path) -> (AnchorConfig, PathBuf) {
        let config = AnchorConfig {
            data_dir: Some(dir.join("data")),
            ..AnchorConfig::default()
        };
        (config, dir.join("anchor.toml"))
    }

    fn repo(dir: &Path, name: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("CODEOWNERS"), "* @acme/payments\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn charge() {}\n").unwrap();
        root
    }

    #[test]
    fn kinds_are_inferred_from_what_the_path_holds() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(infer_kind(&repo(dir.path(), "svc")), SourceKind::Repo);
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("page.md"), "# Page\n").unwrap();
        std::fs::write(wiki.join("page.comments.json"), "[]").unwrap();
        assert_eq!(infer_kind(&wiki), SourceKind::Wiki);
        let org = dir.path().join("org.yaml");
        std::fs::write(&org, "teams: []\n").unwrap();
        assert_eq!(infer_kind(&org), SourceKind::OrgModel);
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(infer_kind(&plain), SourceKind::Repo);
        // Markdown pages without version control or code: an exported wiki.
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join("runbook.md"),
            "---\ntitle: Runbook\n---\n# Runbook\n",
        )
        .unwrap();
        assert_eq!(infer_kind(&notes), SourceKind::Wiki);
        // A README next to code is still a repository.
        std::fs::write(notes.join("main.py"), "print(1)\n").unwrap();
        assert_eq!(infer_kind(&notes), SourceKind::Repo);
    }

    #[test]
    fn a_source_is_added_ingested_listed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config(dir.path());
        let root = repo(dir.path(), "Payments API");

        let added = run(
            SourceAction::Add {
                location: root.clone(),
                id: None,
                kind: None,
                trust: None,
                depth: Depth::Full,
                no_ingest: false,
            },
            config,
            &path,
        )
        .unwrap();
        assert!(
            added[0].contains("`payments-api` (repo, source_of_truth)"),
            "{added:?}"
        );
        assert!(added[1].contains("inferred"), "{added:?}");
        assert!(added[2].starts_with("Ingested"), "{added:?}");

        let saved = AnchorConfig::load(&path).unwrap();
        assert_eq!(saved.sources.len(), 1);
        assert_eq!(saved.sources[0].id.as_str(), "payments-api");
        let engine = Engine::open(saved.clone()).unwrap();
        assert!(engine.store().stats().unwrap().node_count > 0);
        drop(engine);

        let listed = run(SourceAction::List, saved.clone(), &path).unwrap();
        assert!(listed[0].starts_with("ID"));
        assert!(listed[1].starts_with("payments-api"));

        // The same location or id cannot be added twice.
        let again = new_source(&saved, &root, None, None, None, Depth::Full);
        assert!(again.unwrap_err().to_string().contains("already covers"));

        let removed = run(
            SourceAction::Remove {
                id: "payments-api".into(),
                keep_data: false,
            },
            saved,
            &path,
        )
        .unwrap();
        assert!(removed[1].contains("ingested units"), "{removed:?}");
        let after = AnchorConfig::load(&path).unwrap();
        assert!(after.sources.is_empty());
        let engine = Engine::open(after.clone()).unwrap();
        assert_eq!(engine.store().stats().unwrap().node_count, 0);
        drop(engine);

        let missing = run(
            SourceAction::Remove {
                id: "nope".into(),
                keep_data: true,
            },
            after,
            &path,
        );
        assert!(missing.is_err());
    }

    #[test]
    fn overrides_and_bad_input_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = config(dir.path());
        let root = repo(dir.path(), "docs");
        let source = new_source(
            &config,
            &root,
            Some("eng-wiki"),
            Some(SourceKind::Wiki),
            Some(TrustLevel::ReferenceOnly),
            Depth::Map,
        )
        .unwrap();
        assert_eq!(source.id.as_str(), "eng-wiki");
        assert_eq!(source.kind, SourceKind::Wiki);
        assert_eq!(source.depth, Depth::Map);
        assert!(
            new_source(
                &config,
                &dir.path().join("missing"),
                None,
                None,
                None,
                Depth::Full
            )
            .is_err()
        );
        assert!(
            new_source(
                &config,
                &root,
                None,
                Some(SourceKind::Derived),
                None,
                Depth::Full
            )
            .is_err()
        );
    }
}
