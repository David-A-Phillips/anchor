use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::base::{EdgeKind, NodeKey, TrustLevel};
use crate::config::AnchorConfig;
use crate::engine::parser::{Language, parse_source};
use crate::engine::{Engine, ScanReport};

fn tiny() -> SynthSpec {
    SynthSpec::preset(Preset::Tiny)
}

/// Hash of every file under `root`, with the output directory's absolute path blanked out (it
/// appears only in `anchor.toml`).
fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    let canonical = std::fs::canonicalize(root).expect("canonical root");
    let needle = canonical.to_string_lossy().into_owned();
    let mut out = BTreeMap::new();
    let mut stack = vec![canonical.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).expect("read");
                let text = String::from_utf8_lossy(&bytes).replace(&needle, "<OUT>");
                let rel = path
                    .strip_prefix(&canonical)
                    .expect("prefix")
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, blake3::hash(text.as_bytes()).to_hex().to_string());
            }
        }
    }
    out
}

struct Ingested {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    truth: GroundTruth,
    reports: Vec<ScanReport>,
}

fn ingest(spec: &SynthSpec) -> Ingested {
    let dir = tempfile::tempdir().expect("tempdir");
    generate(spec, dir.path()).expect("generate");
    let text = std::fs::read_to_string(dir.path().join("anchor.toml")).expect("anchor.toml");
    let config: AnchorConfig = toml::from_str(&text).expect("valid anchor.toml");
    let engine = Engine::open(config).expect("engine opens");
    let reports = engine.scan_all().expect("scan");
    let truth: GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).expect("truth"),
    )
    .expect("truth parses");
    Ingested {
        _dir: dir,
        engine,
        truth,
        reports,
    }
}

fn node_id(engine: &Engine, key: &str) -> crate::base::NodeId {
    engine
        .store()
        .node_by_key(&NodeKey::new(key))
        .expect("store read")
        .unwrap_or_else(|| panic!("node `{key}` missing from the store"))
        .id
}

/// Whether an edge of `kind` exists between `from` and `to` in the given direction.
fn has_edge(engine: &Engine, from: &str, to: &str, kind: &str, forward: bool) -> bool {
    let (a, b) = (node_id(engine, from), node_id(engine, to));
    engine
        .store()
        .edges_of(a, 100_000)
        .expect("edges")
        .iter()
        .any(|edge| {
            let direction = if forward {
                edge.from == a && edge.to == b
            } else {
                edge.from == b && edge.to == a
            };
            direction && format!("{:?}", edge.kind) == kind
        })
}

#[test]
fn same_spec_gives_identical_trees_and_other_seeds_differ() {
    let (a, b, c) = (
        tempfile::tempdir().expect("dir"),
        tempfile::tempdir().expect("dir"),
        tempfile::tempdir().expect("dir"),
    );
    let first = generate(&tiny(), a.path()).expect("generate");
    let second = generate(&tiny(), b.path()).expect("generate");
    assert_eq!(first, second);
    let hashes = tree_hashes(a.path());
    assert!(hashes.len() > 50, "only {} files", hashes.len());
    assert_eq!(hashes, tree_hashes(b.path()));
    generate(&tiny().seed(99), c.path()).expect("generate");
    assert_ne!(hashes, tree_hashes(c.path()));
}

#[test]
fn generated_tree_ingests_without_warnings() {
    let ingested = ingest(&tiny());
    let warnings: Vec<String> = ingested
        .reports
        .iter()
        .flat_map(|r| r.warnings.iter().map(|w| format!("{}: {w}", r.source)))
        .collect();
    assert!(warnings.is_empty(), "warnings: {warnings:#?}");
    let sources: Vec<&str> = ingested.reports.iter().map(|r| r.source.as_str()).collect();
    for expected in [
        SOURCE_ORG,
        SOURCE_WIKI,
        SOURCE_PORTAL,
        SOURCE_JIRA,
        SOURCE_REVIEWS,
    ] {
        assert!(sources.contains(&expected), "{expected} not scanned");
    }
    assert!(ingested.reports.iter().all(|r| r.units_applied > 0));
}

#[test]
fn every_ground_truth_key_exists_in_the_store() {
    let ingested = ingest(&tiny());
    let missing: Vec<&str> = ingested
        .truth
        .all_keys()
        .into_iter()
        .filter(|key| {
            ingested
                .engine
                .store()
                .node_by_key(&NodeKey::new(*key))
                .expect("store read")
                .is_none()
        })
        .collect();
    assert!(missing.is_empty(), "missing keys: {missing:#?}");
    assert!(!ingested.truth.ownership.is_empty());
    assert!(!ingested.truth.justification.is_empty());
    assert!(!ingested.truth.relationships.is_empty());
}

#[test]
fn every_question_category_is_present() {
    let report = generate(&tiny(), tempfile::tempdir().expect("dir").path()).expect("generate");
    for category in [
        "ownership.service",
        "ownership.api",
        "ownership.component",
        "ownership.file",
        "ownership.endpoint",
        "justification.ticket_blocked",
        "justification.api_not_approved",
        "justification.endpoint_blocked",
        "justification.pull_request_blocked",
        "justification.review_dismissed",
        "justification.thread_dismissed",
        "relationship.service_portfolio",
        "relationship.service_team_lead",
        "relationship.api_consumers",
        "relationship.endpoint_domain",
    ] {
        assert!(
            report
                .questions_by_category
                .get(category)
                .copied()
                .unwrap_or(0)
                > 0,
            "no `{category}` question: {:?}",
            report.questions_by_category
        );
    }
}

#[test]
fn ownership_scenarios_match_the_graph() {
    let ingested = ingest(&tiny());
    let engine = &ingested.engine;
    let mut seen = std::collections::BTreeSet::new();
    for q in &ingested.truth.ownership {
        seen.insert(q.scenario);
        // The expected owner owns the subject (directly), unless the answer is inherited.
        match q.scenario {
            OwnershipScenario::Direct
            | OwnershipScenario::Conflict
            | OwnershipScenario::ReferenceOnly => {
                // Endpoints are owned through their API.
                let owned = if q.kind == OwnershipKind::Endpoint {
                    &q.evidence_keys[1]
                } else {
                    &q.subject_key
                };
                for owner in &q.expected_owner_keys {
                    assert!(
                        has_edge(engine, owner, owned, "Owns", true),
                        "{}: {owner} does not own {}",
                        q.id,
                        q.subject_key
                    );
                }
            }
            OwnershipScenario::Inherited => {
                let component = &q.evidence_keys[1];
                assert!(
                    has_edge(engine, component, &q.subject_key, "Contains", true),
                    "{}",
                    q.id
                );
                assert!(
                    has_edge(engine, &q.expected_owner_keys[0], component, "Owns", true),
                    "{}",
                    q.id
                );
            }
        }
    }
    for scenario in [
        OwnershipScenario::Direct,
        OwnershipScenario::Inherited,
        OwnershipScenario::Conflict,
        OwnershipScenario::ReferenceOnly,
    ] {
        assert!(seen.contains(&scenario), "no {scenario:?} question");
    }
}

#[test]
fn conflict_cases_have_two_contradicting_owns_claims_at_different_trust() {
    let ingested = ingest(&tiny());
    let engine = &ingested.engine;
    let conflicts: Vec<&OwnershipQuestion> = ingested
        .truth
        .ownership
        .iter()
        .filter(|q| q.scenario == OwnershipScenario::Conflict)
        .collect();
    assert!(!conflicts.is_empty());
    for q in conflicts {
        let service = node_id(engine, &q.subject_key);
        let owns: Vec<_> = engine
            .store()
            .edges_of(service, 10_000)
            .expect("edges")
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Owns && e.to == service)
            .collect();
        let right = node_id(engine, &q.expected_owner_keys[0]);
        let wrong = node_id(engine, &q.stale_claim_keys[0]);
        let winner = owns
            .iter()
            .find(|e| e.from == right)
            .expect("true owner edge");
        let stale = owns
            .iter()
            .find(|e| e.from == wrong)
            .expect("stale owner edge");
        assert_eq!(winner.trust, TrustLevel::SourceOfTruth, "{}", q.id);
        assert_eq!(stale.trust, TrustLevel::ReferenceOnly, "{}", q.id);
        assert_ne!(right, wrong);
        assert!(
            q.stale_claim_keys.iter().any(|k| k.starts_with("wiki:")),
            "{}",
            q.id
        );
    }
}

#[test]
fn reference_only_cases_have_no_authoritative_owner() {
    let ingested = ingest(&tiny());
    let engine = &ingested.engine;
    let cases: Vec<&OwnershipQuestion> = ingested
        .truth
        .ownership
        .iter()
        .filter(|q| q.scenario == OwnershipScenario::ReferenceOnly)
        .collect();
    assert!(!cases.is_empty());
    for q in cases {
        let service = node_id(engine, &q.subject_key);
        let owns: Vec<_> = engine
            .store()
            .edges_of(service, 10_000)
            .expect("edges")
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Owns && e.to == service)
            .collect();
        assert_eq!(owns.len(), 1, "{}", q.id);
        assert_eq!(owns[0].trust, TrustLevel::ReferenceOnly);
    }
}

#[test]
fn declared_multi_hop_paths_exist_as_edges() {
    let ingested = ingest(&tiny());
    let engine = &ingested.engine;
    for q in &ingested.truth.relationships {
        assert_eq!(q.paths.len(), q.expected_keys.len(), "{}", q.id);
        for (path, expected) in q.paths.iter().zip(&q.expected_keys) {
            assert_eq!(
                path.first().map(|s| s.key.as_str()),
                Some(q.subject_key.as_str())
            );
            assert_eq!(path.last().map(|s| s.key.as_str()), Some(expected.as_str()));
            for pair in path.windows(2) {
                let kind = pair[1].edge.as_deref().expect("edge kind");
                assert!(
                    has_edge(engine, &pair[0].key, &pair[1].key, kind, pair[1].forward)
                        || has_edge(engine, &pair[1].key, &pair[0].key, kind, !pair[1].forward),
                    "{}: no {kind} edge {} -> {}",
                    q.id,
                    pair[0].key,
                    pair[1].key
                );
            }
        }
    }
}

#[test]
fn blocking_edges_and_dismissals_are_in_the_graph() {
    let ingested = ingest(&tiny());
    let engine = &ingested.engine;
    for q in &ingested.truth.justification {
        match q.kind {
            JustificationKind::TicketBlocked => {
                assert!(
                    has_edge(engine, &q.expected_keys[0], &q.subject_key, "Blocks", true),
                    "{}",
                    q.id
                );
            }
            JustificationKind::ApiNotApproved
            | JustificationKind::EndpointBlocked
            | JustificationKind::PullRequestBlocked => {
                let approval = node_id(engine, &q.expected_keys[0]);
                let subject = node_id(engine, &q.subject_key);
                let edges = engine.store().edges_of(approval, 1000).expect("edges");
                let linked = edges.iter().any(|e| e.to == subject || e.from == subject);
                assert!(linked, "{}: approval not linked to subject", q.id);
            }
            JustificationKind::ReviewDismissed | JustificationKind::ThreadDismissed => {}
        }
        assert!(!q.key_phrases.is_empty());
    }
}

#[test]
fn presets_and_scaling_produce_the_requested_counts() {
    for (preset, repos) in [
        (Preset::Tiny, 8),
        (Preset::Small, 24),
        (Preset::Medium, 100),
    ] {
        let spec = SynthSpec::preset(preset);
        assert_eq!(spec.repositories(), repos, "{preset:?}");
    }
    assert_eq!(SynthSpec::preset(Preset::Large).repositories(), 1000);
    for repos in [10, 37, 100, 1000] {
        assert_eq!(
            SynthSpec::default().with_repositories(repos).repositories(),
            repos
        );
    }
    let dir = tempfile::tempdir().expect("dir");
    let spec = SynthSpec::preset(Preset::Small);
    let report = generate(&spec, dir.path()).expect("generate");
    assert_eq!(report.repositories, 24);
    assert_eq!(report.teams, spec.teams);
    assert_eq!(report.people, spec.people);
    assert_eq!(report.wiki_pages, spec.wiki_pages);
    assert_eq!(report.tickets, spec.tickets);
    assert_eq!(report.apis, spec.apis);
    assert_eq!(report.pull_requests, spec.pull_requests);
    assert_eq!(report.source_files, 24 * spec.files_per_service);
    let repo_dirs = std::fs::read_dir(dir.path().join("repos"))
        .expect("repos")
        .count();
    assert_eq!(repo_dirs, 24);
    assert!(report.total_bytes() > 0 && report.total_questions() > 0);
    assert_eq!(
        report
            .bytes_by_kind
            .keys()
            .filter(|k| *k == "ground_truth")
            .count(),
        1
    );
}

#[test]
fn generated_code_parses_without_syntax_errors_in_every_language() {
    let spec = SynthSpec::preset(Preset::Tiny).languages(
        SynthLanguage::ALL
            .iter()
            .map(|&language| LanguageWeight {
                language,
                weight: 1,
            })
            .collect(),
    );
    let dir = tempfile::tempdir().expect("dir");
    generate(&spec, dir.path()).expect("generate");
    let mut languages = std::collections::BTreeSet::new();
    let mut stack = vec![dir.path().join("repos")];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.to_string_lossy().into_owned();
            let Some(language) = Language::from_path(&rel) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).expect("read");
            let parsed = parse_source(language, &text).expect("parse");
            assert!(
                parsed.syntax_errors.is_empty(),
                "{rel}: {:?}\n{text}",
                parsed.syntax_errors
            );
            assert!(!parsed.symbols.is_empty(), "{rel}: no symbols");
            languages.insert(format!("{language}"));
        }
    }
    assert_eq!(languages.len(), 6, "{languages:?}");
}

#[test]
fn ownership_file_formats_are_all_exercised() {
    let dir = tempfile::tempdir().expect("dir");
    generate(&tiny(), dir.path()).expect("generate");
    let mut found = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(dir.path().join("repos")).expect("repos") {
        let repo = entry.expect("entry").path();
        for name in [
            "CODEOWNERS",
            "MAINTAINERS",
            "OWNERS",
            "openapi.yaml",
            "README.md",
        ] {
            if repo.join(name).exists() {
                found.insert(name);
            }
        }
    }
    assert_eq!(found.len(), 5, "{found:?}");
}

#[test]
fn timestamps_are_calendar_correct() {
    use super::plan::timestamp;
    assert_eq!(timestamp(0, 0), "2025-01-01T00:00:00Z");
    assert_eq!(timestamp(59, 3661), "2025-03-01T01:01:01Z");
    assert_eq!(timestamp(365 + 59, 0), "2026-03-01T00:00:00Z");
}

#[test]
fn small_preset_with_another_seed_ingests_cleanly_and_keys_resolve() {
    let ingested = ingest(&SynthSpec::preset(Preset::Small).seed(7));
    let warnings: Vec<String> = ingested
        .reports
        .iter()
        .flat_map(|r| r.warnings.iter().map(|w| format!("{}: {w}", r.source)))
        .collect();
    assert!(warnings.is_empty(), "warnings: {warnings:#?}");
    for key in ingested.truth.all_keys() {
        assert!(
            ingested
                .engine
                .store()
                .node_by_key(&NodeKey::new(key))
                .expect("read")
                .is_some(),
            "missing {key}"
        );
    }
}

#[test]
#[ignore = "generates 1000 repositories; run explicitly"]
fn large_preset_generates_a_thousand_repositories() {
    let dir = tempfile::tempdir().expect("dir");
    let report = generate(&SynthSpec::preset(Preset::Large), dir.path()).expect("generate");
    assert_eq!(report.repositories, 1000);
}
