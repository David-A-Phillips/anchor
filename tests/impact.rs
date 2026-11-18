//! Change impact and hotspots on a generated organisation, checked against its ground truth:
//! every consumer of an API is affected by a change to it, a change to a file reaches its
//! service and the code that calls into it, and hotspots count consumers and flag the services
//! owned by a single person.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anchor::base::{EdgeKind, NodeKey, NodeKind};
use anchor::config::AnchorConfig;
use anchor::engine::Engine;
use anchor::engine::impact::{HotspotOptions, ImpactOptions};
use anchor::synth::{
    self, GroundTruth, OwnershipKind, OwnershipScenario, Preset, RelationshipKind, SynthSpec,
};

const SEED: u64 = 77;

struct World {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    truth: GroundTruth,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    synth::generate(&SynthSpec::preset(Preset::Small).seed(SEED), dir.path()).unwrap();
    let config = AnchorConfig::from_toml_str(
        &std::fs::read_to_string(dir.path().join("anchor.toml")).unwrap(),
    )
    .unwrap();
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let truth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();
    World {
        _dir: dir,
        engine,
        truth,
    }
}

fn keys_of_kind(report: &anchor::engine::impact::ImpactReport, kind: NodeKind) -> BTreeSet<String> {
    report
        .dependents
        .iter()
        .filter(|d| d.entity.kind == kind)
        .map(|d| d.entity.key.clone())
        .collect()
}

fn options(limit: usize, kinds: &[NodeKind]) -> ImpactOptions {
    ImpactOptions {
        limit,
        kinds: kinds.to_vec(),
        ..ImpactOptions::default()
    }
}

#[test]
fn a_change_to_an_api_affects_every_service_that_consumes_it() {
    let w = world();
    let consumers: Vec<_> = w
        .truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
        .collect();
    assert!(!consumers.is_empty());
    let mut union = BTreeSet::new();
    for question in &consumers {
        let report = w
            .engine
            .impact(
                std::slice::from_ref(&question.subject_key),
                &options(500, &[NodeKind::Service]),
            )
            .unwrap();
        let found = keys_of_kind(&report, NodeKind::Service);
        for expected in &question.expected_keys {
            assert!(
                found.contains(expected),
                "{} should affect {expected}; found {found:?}",
                question.subject_key
            );
            let dependent = report
                .dependents
                .iter()
                .find(|d| &d.entity.key == expected)
                .unwrap();
            assert_eq!(dependent.target, question.subject_key);
            assert!(
                dependent.via.starts_with(&question.subject_key),
                "{}",
                dependent.via
            );
            assert!(dependent.via.contains("<-consumes-"), "{}", dependent.via);
        }
        union.extend(question.expected_keys.iter().cloned());
        assert!(!report.truncated);
    }

    // Asking about every API at once covers every consumer, in one report.
    let all: Vec<String> = consumers.iter().map(|q| q.subject_key.clone()).collect();
    let together = w
        .engine
        .impact(&all, &options(5_000, &[NodeKind::Service]))
        .unwrap();
    let found = keys_of_kind(&together, NodeKind::Service);
    assert!(
        union.is_subset(&found),
        "missing {:?}",
        union.difference(&found)
    );
    assert_eq!(together.targets.len(), all.len());
    assert!(together.targets.iter().all(|t| t.entity.is_some()));
}

#[test]
fn a_change_to_a_file_reaches_its_service_and_code_in_other_files_that_calls_into_it() {
    let w = world();
    // Find a call from a symbol in one file to a symbol in another file.
    let graph = w.engine.graph();
    let store = w.engine.store();
    let (caller, callee) = {
        let graph = graph.read();
        let mut found = None;
        for edge in graph.edges().filter(|e| e.kind == EdgeKind::Calls) {
            let from = store.node(edge.from).unwrap().unwrap();
            let to = store.node(edge.to).unwrap().unwrap();
            let file = |key: &str| key.split('#').next().unwrap().to_owned();
            if from.kind == NodeKind::Symbol
                && to.kind == NodeKind::Symbol
                && file(from.key.as_str()) != file(to.key.as_str())
            {
                found = Some((from, to));
                break;
            }
        }
        found.expect("the generated code has cross-file calls")
    };
    let callee_file = callee
        .key
        .as_str()
        .split('#')
        .next()
        .unwrap()
        .replacen("symbol:", "file:", 1);
    let file = store
        .node_by_key(&NodeKey::new(&callee_file))
        .unwrap()
        .unwrap();

    let report = w
        .engine
        .impact(&[file.key.to_string()], &options(1_000, &[]))
        .unwrap();
    let caller_hit = report
        .dependents
        .iter()
        .find(|d| d.entity.key == caller.key.as_str())
        .unwrap_or_else(|| panic!("{} should be affected: {report:#?}", caller.key));
    assert_eq!(
        caller_hit.hops, 1,
        "the callee symbol is a seed: {}",
        caller_hit.via
    );
    assert!(caller_hit.via.contains("<-calls-"));
    // The file's own service is affected one step up.
    let service = report
        .dependents
        .iter()
        .find(|d| d.entity.kind == NodeKind::Service && d.hops == 1)
        .unwrap_or_else(|| panic!("no enclosing service: {report:#?}"));
    assert!(service.via.contains("<-contains-"));
    // Siblings in the same service are not implicated through containment.
    assert!(
        !report
            .dependents
            .iter()
            .any(|d| d.via.contains("-contains->"))
    );
    // Somebody owns the file.
    assert!(
        report
            .owners
            .iter()
            .any(|o| o.owns.iter().any(|k| k == file.key.as_str())),
        "{report:#?}"
    );
    assert_eq!(report.total, report.summary.values().sum::<usize>());
}

#[test]
fn listing_is_capped_but_counting_is_not_and_bad_targets_are_reported() {
    let w = world();
    let api = w
        .truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
        .max_by_key(|q| q.expected_keys.len())
        .unwrap()
        .subject_key
        .clone();
    let wide = w
        .engine
        .impact(std::slice::from_ref(&api), &options(1_000, &[]))
        .unwrap();
    assert!(wide.total >= 2, "{wide:#?}");
    let narrow = w
        .engine
        .impact(std::slice::from_ref(&api), &options(1, &[]))
        .unwrap();
    assert_eq!(narrow.dependents.len(), 1);
    assert_eq!(narrow.total, wide.total);
    assert_eq!(narrow.summary, wide.summary);
    // Nearest first.
    assert!(wide.dependents.windows(2).all(|p| p[0].hops <= p[1].hops));
    assert_eq!(narrow.dependents[0].hops, wide.dependents[0].hops);

    let mixed = w
        .engine
        .impact(&[api, "no/such/file.rs".into()], &options(10, &[]))
        .unwrap();
    assert!(mixed.targets.iter().any(|t| t.problem.is_some()));
    assert!(mixed.targets.iter().any(|t| t.entity.is_some()));
    assert!(
        w.engine
            .impact(&["no-such-thing-at-all".into()], &options(10, &[]))
            .is_err()
    );
    assert!(w.engine.impact(&[], &options(10, &[])).is_err());
}

#[test]
fn a_name_shared_by_a_service_its_api_and_its_component_means_the_service() {
    let w = world();
    let service = w
        .truth
        .ownership
        .iter()
        .find(|q| q.kind == OwnershipKind::Service)
        .unwrap()
        .subject_key
        .clone();
    let name = service.trim_start_matches("service:").to_owned();
    let report = w.engine.impact(&[name], &options(5, &[])).unwrap();
    assert_eq!(report.targets[0].entity.as_ref().unwrap().key, service);
}

#[test]
fn hotspots_count_consumers_and_flag_services_owned_by_one_person() {
    let w = world();
    let report = w
        .engine
        .hotspots(&HotspotOptions {
            kind: NodeKind::Service,
            limit: 10_000,
            min_dependents: 1,
        })
        .unwrap();
    assert!(!report.hotspots.is_empty());
    assert!(!report.truncated);
    let by_key: HashMap<&str, &anchor::engine::impact::Hotspot> = report
        .hotspots
        .iter()
        .map(|h| (h.entity.key.as_str(), h))
        .collect();
    // Ranked by dependents.
    assert!(
        report
            .hotspots
            .windows(2)
            .all(|pair| pair[0].dependents >= pair[1].dependents)
    );

    // A service implementing an API has at least as many dependents as the API has consumers.
    for question in w
        .truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
    {
        let service = question.subject_key.replacen("api:", "service:", 1);
        let hotspot = by_key
            .get(service.as_str())
            .unwrap_or_else(|| panic!("{service} has consumers but is not a hotspot"));
        assert!(
            hotspot.dependents >= question.expected_keys.len(),
            "{service}: {} < {}",
            hotspot.dependents,
            question.expected_keys.len()
        );
    }

    // Owners match the ground truth for services owned directly.
    let mut checked = 0;
    for question in w
        .truth
        .ownership
        .iter()
        .filter(|q| q.kind == OwnershipKind::Service && q.scenario == OwnershipScenario::Direct)
    {
        let Some(hotspot) = by_key.get(question.subject_key.as_str()) else {
            continue;
        };
        let owner = hotspot.owner.as_ref().expect("directly owned");
        // Aliases merge people, so compare nodes rather than primary keys.
        let owner_node = w
            .engine
            .store()
            .node_by_key(&NodeKey::new(&owner.owner.key))
            .unwrap()
            .unwrap();
        assert!(
            question
                .expected_owner_keys
                .iter()
                .any(|key| owner_node.keys.iter().any(|k| k.as_str() == key)),
            "{}: owner {:?} not in {:?}",
            question.subject_key,
            owner_node.keys,
            question.expected_owner_keys
        );
        let by_person = owner.owner.kind == NodeKind::Person;
        assert_eq!(
            hotspot.risks.iter().any(|r| r == "single_person_owner"),
            by_person,
            "{}",
            question.subject_key
        );
        checked += 1;
    }
    assert!(checked > 0);
    assert_eq!(
        report.single_person,
        report
            .hotspots
            .iter()
            .filter(|h| h.risks.iter().any(|r| r == "single_person_owner"))
            .count()
    );

    // Owners are ranked by load, and the counts add up.
    assert!(report.owners.windows(2).all(|p| p[0].owns >= p[1].owns));
    for owner in &report.owners {
        assert_eq!(owner.owns, owner.by_kind.values().sum::<usize>());
    }
    assert_eq!(
        report.owners.iter().map(|o| o.hotspots).sum::<usize>(),
        report
            .hotspots
            .iter()
            .filter(|h| h.owner.as_ref().is_some_and(|o| report
                .owners
                .iter()
                .any(|load| load.owner.key == o.owner.key)))
            .count()
    );
}

#[test]
fn hotspots_work_at_file_and_symbol_granularity_and_respect_their_options() {
    let w = world();
    for kind in [NodeKind::File, NodeKind::Symbol] {
        let report = w
            .engine
            .hotspots(&HotspotOptions {
                kind,
                limit: 5,
                min_dependents: 1,
            })
            .unwrap();
        assert!(!report.hotspots.is_empty(), "{kind}");
        assert!(report.hotspots.len() <= 5);
        assert!(report.hotspots.iter().all(|h| h.entity.kind == kind));
        assert!(report.owners.len() <= 5);
    }
    let strict = w
        .engine
        .hotspots(&HotspotOptions {
            kind: NodeKind::Service,
            limit: 100,
            min_dependents: 1_000,
        })
        .unwrap();
    assert!(strict.hotspots.is_empty());
}

#[test]
fn reports_render_as_text() {
    let w = world();
    let api = w
        .truth
        .relationships
        .iter()
        .find(|q| q.kind == RelationshipKind::ApiConsumers)
        .unwrap();
    let report = w
        .engine
        .impact(std::slice::from_ref(&api.subject_key), &options(5, &[]))
        .unwrap();
    let text = anchor::engine::impact::render_impact(&report);
    assert!(text.contains(&api.subject_key));
    assert!(text.contains("Affected, directly or through what depends on them:"));
    let hotspots = w.engine.hotspots(&HotspotOptions::default()).unwrap();
    let text = anchor::engine::impact::render_hotspots(&hotspots);
    assert!(text.contains("Most depended-on services"));
    assert!(text.contains("Owners by entities owned directly"));
}

#[test]
fn a_team_target_stands_for_every_service_it_owns_and_reaches_their_consumers() {
    let w = world();
    let store = w.engine.store();
    // Services by owning team, from the ground truth: owned directly or through a component.
    let mut owned_by: HashMap<String, BTreeSet<String>> = HashMap::new();
    for question in w.truth.ownership.iter().filter(|q| {
        q.kind == OwnershipKind::Service
            && matches!(
                q.scenario,
                OwnershipScenario::Direct | OwnershipScenario::Inherited
            )
    }) {
        for owner in &question.expected_owner_keys {
            if owner.starts_with("team:") {
                owned_by
                    .entry(owner.clone())
                    .or_default()
                    .insert(question.subject_key.clone());
            }
        }
    }
    assert!(!owned_by.is_empty());
    let consumers: HashMap<String, Vec<String>> = w
        .truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
        .map(|q| {
            (
                q.subject_key.replacen("api:", "service:", 1),
                q.expected_keys.clone(),
            )
        })
        .collect();

    let mut reached = 0;
    for (team, services) in &owned_by {
        let report = w
            .engine
            .impact(std::slice::from_ref(team), &options(5_000, &[]))
            .unwrap();
        let target = &report.targets[0];
        assert_eq!(target.entity.as_ref().unwrap().kind, NodeKind::Team);
        let covers: BTreeSet<&str> = target.covers.iter().map(String::as_str).collect();
        for service in services {
            assert!(covers.contains(service.as_str()), "{team} owns {service}");
        }
        // Consumers of the team's APIs are affected, unless the team owns them too.
        let affected = keys_of_kind(&report, NodeKind::Service);
        for service in services {
            for consumer in consumers.get(service).into_iter().flatten() {
                if covers.contains(consumer.as_str()) {
                    continue;
                }
                assert!(
                    affected.contains(consumer),
                    "{team}: {consumer} consumes the API of {service}"
                );
                reached += 1;
            }
        }
        // What the team stands for is not reported as depending on it.
        assert!(
            report
                .dependents
                .iter()
                .all(|d| !covers.contains(d.entity.key.as_str()))
        );
    }
    assert!(reached > 0, "no team's API has consumers outside the team");

    // A person resolves by name, and stands for what they own.
    let person = w
        .truth
        .ownership
        .iter()
        .flat_map(|q| &q.expected_owner_keys)
        .find(|key| key.starts_with("person:"))
        .expect("a person owns something");
    let node = store.node_by_key(&NodeKey::new(person)).unwrap().unwrap();
    let name = store.names(&[node.id]).unwrap()[&node.id].1.clone();
    let report = w.engine.impact(&[name], &options(50, &[])).unwrap();
    let target = report.targets[0].entity.as_ref().unwrap();
    assert_eq!(target.kind, NodeKind::Person);
    assert!(!report.targets[0].covers.is_empty());
}

#[test]
fn hotspots_report_who_changed_each_service_as_git_records_it() {
    if !synth::git_available() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let spec = SynthSpec::preset(Preset::Small).seed(SEED);
    synth::generate(&spec, dir.path()).unwrap();
    let history = synth::init_git(&spec, dir.path()).unwrap();
    assert!(history.commits > history.repositories);
    let config = AnchorConfig::from_toml_str(
        &std::fs::read_to_string(dir.path().join("anchor.toml")).unwrap(),
    )
    .unwrap();
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let report = engine
        .hotspots(&HotspotOptions {
            kind: NodeKind::Service,
            limit: 10_000,
            min_dependents: 1,
        })
        .unwrap();
    assert!(report.history);
    let mut single = 0;
    for hotspot in &report.hotspots {
        let slug = hotspot.entity.key.trim_start_matches("service:");
        let repo = dir.path().join("repos").join(slug);
        if !repo.is_dir() {
            continue;
        }
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--format=%ae"])
            .output()
            .unwrap();
        let emails: Vec<String> = String::from_utf8_lossy(&log.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        let authors: BTreeSet<&String> = emails.iter().collect();
        assert_eq!(hotspot.commits, emails.len(), "{slug}: commits");
        assert_eq!(
            hotspot.committers.len(),
            authors.len(),
            "{slug}: committers"
        );
        assert_eq!(
            hotspot.committers.iter().map(|c| c.commits).sum::<usize>(),
            emails.len(),
            "{slug}: commits by committer"
        );
        let flagged = hotspot.risks.iter().any(|r| r == "single_committer");
        assert_eq!(flagged, authors.len() == 1, "{slug}: single committer flag");
        single += usize::from(flagged);
    }
    assert_eq!(single, report.single_committer);
    assert!(
        anchor::engine::impact::render_hotspots(&report).contains("changed in"),
        "committers are rendered"
    );

    // Without history nothing is claimed about committers.
    let w = world();
    let report = w.engine.hotspots(&HotspotOptions::default()).unwrap();
    assert!(!report.history);
    assert!(
        report
            .hotspots
            .iter()
            .all(|h| h.committers.is_empty() && !h.risks.iter().any(|r| r == "single_committer"))
    );
}

#[test]
fn an_impact_report_renders_as_a_pull_request_comment() {
    let w = world();
    let question = w
        .truth
        .relationships
        .iter()
        .find(|q| q.kind == RelationshipKind::ApiConsumers)
        .unwrap();
    let report = w
        .engine
        .impact(
            &[question.subject_key.clone(), "no/such/file.rs".to_owned()],
            &options(500, &[]),
        )
        .unwrap();
    let text = anchor::engine::impact::render_impact_markdown(&report);
    assert!(text.starts_with("### Change impact\n"), "{text}");
    assert!(text.contains("**Owners to notify**"), "{text}");
    assert!(text.contains("| Hops | Dependent | Kind | Via |"), "{text}");
    for consumer in &question.expected_keys {
        assert!(
            text.contains(&format!("`{consumer}`")),
            "{consumer} listed: {text}"
        );
    }
    assert!(
        text.contains("Not in the graph (nothing to follow): `no/such/file.rs`"),
        "{text}"
    );
    // Table rows stay one row each.
    for line in text
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Hops"))
    {
        assert_eq!(line.matches(" | ").count(), 3, "{line}");
    }
}

/// Impact and hotspot accuracy against the ground truth, printed as the table in
/// `docs/benchmarks.md`. Runs on the small preset by default; for the published figures:
/// `ANCHOR_IMPACT_SCALE=medium ANCHOR_IMPACT_SEED=7 cargo test --release --test impact scores -- --nocapture`.
#[test]
fn impact_and_hotspot_scores_against_the_ground_truth() {
    let scale = std::env::var("ANCHOR_IMPACT_SCALE").unwrap_or_else(|_| "small".to_owned());
    let seed: u64 = std::env::var("ANCHOR_IMPACT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEED);
    let preset = match scale.as_str() {
        "tiny" => Preset::Tiny,
        "medium" => Preset::Medium,
        _ => Preset::Small,
    };
    let dir = tempfile::tempdir().unwrap();
    synth::generate(&SynthSpec::preset(preset).seed(seed), dir.path()).unwrap();
    let config = AnchorConfig::from_toml_str(
        &std::fs::read_to_string(dir.path().join("anchor.toml")).unwrap(),
    )
    .unwrap();
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let truth: GroundTruth = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("ground_truth.json")).unwrap(),
    )
    .unwrap();
    let store = engine.store();
    let same = |a: &str, b: &str| {
        let (Some(a), Some(b)) = (
            store.node_by_key(&NodeKey::new(a)).unwrap(),
            store.node_by_key(&NodeKey::new(b)).unwrap(),
        ) else {
            return false;
        };
        a.id == b.id
    };

    // 1. API consumers: a change to an API reaches every service consuming it (recall); the
    //    services reported as consuming it directly are consumers (precision).
    let (mut expected, mut found, mut direct, mut direct_right) = (0, 0, 0, 0);
    let mut millis = Vec::new();
    for question in truth
        .relationships
        .iter()
        .filter(|q| q.kind == RelationshipKind::ApiConsumers)
    {
        let started = std::time::Instant::now();
        let report = engine
            .impact(
                std::slice::from_ref(&question.subject_key),
                &options(5_000, &[NodeKind::Service]),
            )
            .unwrap();
        millis.push(started.elapsed().as_secs_f64() * 1000.0);
        let affected = keys_of_kind(&report, NodeKind::Service);
        expected += question.expected_keys.len();
        found += question
            .expected_keys
            .iter()
            .filter(|k| affected.contains(*k))
            .count();
        for dependent in report.dependents.iter().filter(|d| {
            d.hops == 1 && d.via.contains("<-consumes-") && d.entity.kind == NodeKind::Service
        }) {
            direct += 1;
            direct_right += usize::from(question.expected_keys.contains(&dependent.entity.key));
        }
    }

    // 2. Owners to notify: the owner reported for a changed API or service is its owner.
    let (mut owner_checked, mut owner_right) = (0, 0);
    for question in truth.ownership.iter().filter(|q| {
        matches!(q.kind, OwnershipKind::Api | OwnershipKind::Service)
            && matches!(
                q.scenario,
                OwnershipScenario::Direct | OwnershipScenario::Conflict
            )
    }) {
        let report = engine
            .impact(
                std::slice::from_ref(&question.subject_key),
                &options(1, &[]),
            )
            .unwrap();
        let owner = report
            .owners
            .iter()
            .find(|o| o.owns.iter().any(|k| same(k, &question.subject_key)));
        owner_checked += 1;
        owner_right += usize::from(owner.is_some_and(|o| {
            question
                .expected_owner_keys
                .iter()
                .any(|k| same(k, &o.owner.key))
        }));
    }

    // 3. Hotspots: services flagged as owned by a single person are exactly those whose
    //    ground-truth owner is a person.
    let hotspots = engine
        .hotspots(&HotspotOptions {
            kind: NodeKind::Service,
            limit: 100_000,
            min_dependents: 1,
        })
        .unwrap();
    let flagged: BTreeSet<&str> = hotspots
        .hotspots
        .iter()
        .filter(|h| h.risks.iter().any(|r| r == "single_person_owner"))
        .map(|h| h.entity.key.as_str())
        .collect();
    let listed: BTreeSet<&str> = hotspots
        .hotspots
        .iter()
        .map(|h| h.entity.key.as_str())
        .collect();
    let by_person: BTreeSet<&str> = truth
        .ownership
        .iter()
        .filter(|q| {
            q.kind == OwnershipKind::Service
                && q.scenario == OwnershipScenario::Direct
                && listed.contains(q.subject_key.as_str())
                && q.expected_owner_keys
                    .iter()
                    .all(|k| k.starts_with("person:"))
        })
        .map(|q| q.subject_key.as_str())
        .collect();
    let flag_right = flagged.intersection(&by_person).count();

    millis.sort_by(f64::total_cmp);
    let p95 = millis[(millis.len() * 95 / 100).min(millis.len() - 1)];
    let ratio = |a: usize, b: usize| a as f64 / b.max(1) as f64;
    println!("impact and hotspots, {scale} preset, seed {seed}");
    println!(
        "API consumers reached (recall)        {:.3}  ({found}/{expected})",
        ratio(found, expected)
    );
    println!(
        "direct consumers correct (precision)  {:.3}  ({direct_right}/{direct})",
        ratio(direct_right, direct)
    );
    println!(
        "owner to notify correct               {:.3}  ({owner_right}/{owner_checked})",
        ratio(owner_right, owner_checked)
    );
    println!(
        "single-person owner flag precision    {:.3}  ({flag_right}/{})",
        ratio(flag_right, flagged.len()),
        flagged.len()
    );
    println!(
        "single-person owner flag recall       {:.3}  ({flag_right}/{})",
        ratio(flag_right, by_person.len()),
        by_person.len()
    );
    println!("impact time per API, p95              {p95:.1} ms");

    assert_eq!(found, expected, "every consumer is reached");
    assert_eq!(direct_right, direct, "every direct consumer is a consumer");
    assert!(owner_checked > 0 && ratio(owner_right, owner_checked) >= 0.95);
    assert_eq!(flag_right, flagged.len(), "no false single-person flags");
    assert_eq!(
        flag_right,
        by_person.len(),
        "every single-person service flagged"
    );
}
