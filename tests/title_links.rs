//! Wiki pages that name another page's title are linked to it after a full scan, and the links
//! follow the text as it changes.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use anchor::base::{EdgeKind, NodeKind, SourceKind, TrustLevel};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::router::ProviderKind;
use anchor::engine::title_links::TITLE_LINK_SOURCE;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn engine(dir: &Path, title_links: bool) -> std::sync::Arc<Engine> {
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.graph.title_links = title_links;
    config.sources.push(SourceConfig::new(
        "wiki",
        SourceKind::Wiki,
        dir.join("wiki").to_string_lossy(),
    ));
    Engine::open(config).unwrap()
}

/// Names of the pages `from` mentions.
fn mentioned(engine: &Engine, from: &str) -> Vec<String> {
    let pages = engine
        .store()
        .nodes_by_kind(NodeKind::WikiPage, 100, 0)
        .unwrap();
    let page = pages.iter().find(|p| p.name == from).unwrap();
    let mut names: Vec<String> = engine
        .store()
        .edges_of(page.id, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Mentions && e.from == page.id)
        .filter_map(|e| pages.iter().find(|p| p.id == e.to).map(|p| p.name.clone()))
        .collect();
    names.sort();
    names
}

fn world(dir: &Path) {
    write(
        &dir.join("wiki/retries.md"),
        "# Retry runbook\n\nFailed charges are retried by the Payment gateway. See also Ledger.\n",
    );
    write(
        &dir.join("wiki/gateway.md"),
        "# Payment gateway\n\nWhere charges enter. The payment gateway retries are in the runbook.\n",
    );
    write(&dir.join("wiki/ledger.md"), "# Ledger\n\nThe books.\n");
}

#[test]
fn a_page_naming_another_page_is_linked_to_it() {
    let dir = tempfile::tempdir().unwrap();
    world(dir.path());
    let engine = engine(dir.path(), true);
    let reports = engine.scan_all().unwrap();
    assert!(reports.iter().any(|r| r.source == TITLE_LINK_SOURCE));

    assert_eq!(
        mentioned(&engine, "Retry runbook"),
        vec!["Ledger", "Payment gateway"]
    );
    // "payment gateway" in lower case is not the title, and "runbook" is not "Retry runbook".
    assert!(mentioned(&engine, "Payment gateway").is_empty());

    let edge = engine
        .store()
        .edges_of(
            engine
                .store()
                .nodes_by_kind(NodeKind::WikiPage, 10, 0)
                .unwrap()
                .iter()
                .find(|p| p.name == "Retry runbook")
                .unwrap()
                .id,
            100,
        )
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EdgeKind::Mentions)
        .unwrap();
    assert_eq!(edge.trust, TrustLevel::ReferenceOnly);

    // The text stops naming the gateway: that link goes, the other stays.
    write(
        &dir.path().join("wiki/retries.md"),
        "# Retry runbook\n\nFailed charges are retried. See also Ledger.\n",
    );
    engine.scan_all().unwrap();
    assert_eq!(mentioned(&engine, "Retry runbook"), vec!["Ledger"]);

    // It names nothing at all: its link unit is removed.
    write(
        &dir.path().join("wiki/retries.md"),
        "# Retry runbook\n\nFailed charges are retried.\n",
    );
    engine.scan_all().unwrap();
    assert!(mentioned(&engine, "Retry runbook").is_empty());
}

#[test]
fn title_links_can_be_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    world(dir.path());
    let engine = engine(dir.path(), false);
    let reports = engine.scan_all().unwrap();
    assert!(!reports.iter().any(|r| r.source == TITLE_LINK_SOURCE));
    assert!(mentioned(&engine, "Retry runbook").is_empty());
}

#[test]
fn the_watcher_keeps_title_links_current_without_a_full_scan() {
    let dir = tempfile::tempdir().unwrap();
    world(dir.path());
    std::fs::remove_file(dir.path().join("wiki/ledger.md")).unwrap();
    for i in 0..20 {
        write(
            &dir.path().join(format!("wiki/other{i}.md")),
            &format!("# Other page {i}\n\nUnrelated text.\n"),
        );
    }
    let engine = engine(dir.path(), true);
    engine.scan_all().unwrap();
    assert_eq!(mentioned(&engine, "Retry runbook"), vec!["Payment gateway"]);

    // A new page whose title an existing page already names: the existing page links to it.
    let ledger = dir.path().join("wiki/ledger.md");
    write(&ledger, "# Ledger\n\nThe books.\n");
    assert!(
        engine
            .handle_path_change(&ledger, std::time::Instant::now())
            .unwrap()
    );
    assert_eq!(
        mentioned(&engine, "Retry runbook"),
        vec!["Ledger", "Payment gateway"]
    );

    // An existing page starts naming another page.
    let gateway = dir.path().join("wiki/gateway.md");
    write(
        &gateway,
        "# Payment gateway\n\nPostings go to the Ledger.\n",
    );
    engine
        .handle_path_change(&gateway, std::time::Instant::now())
        .unwrap();
    assert_eq!(mentioned(&engine, "Payment gateway"), vec!["Ledger"]);

    // The incremental pass looks only at the changed page and the pages naming its title.
    let ledger_id = engine
        .store()
        .nodes_by_kind(NodeKind::WikiPage, 100, 0)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "Ledger")
        .unwrap()
        .id;
    let report = engine.relink_titles(&[ledger_id]).unwrap();
    assert_eq!(report.files_seen, 3, "{report:?}");
}

#[test]
fn an_unchanged_rescan_applies_nothing_and_skips_the_title_pass() {
    let dir = tempfile::tempdir().unwrap();
    world(dir.path());
    let engine = engine(dir.path(), true);
    let first = engine.scan_all().unwrap();
    let wiki = |reports: &[anchor::engine::core::ScanReport]| {
        reports
            .iter()
            .find(|r| r.source == "wiki")
            .unwrap()
            .units_applied
    };
    assert_eq!(wiki(&first), 3);

    let again = engine.scan_all().unwrap();
    assert_eq!(wiki(&again), 0, "nothing changed");
    assert!(
        !again.iter().any(|r| r.source == TITLE_LINK_SOURCE),
        "links are current: {again:?}"
    );

    write(
        &dir.path().join("wiki/ledger.md"),
        "# Ledger\n\nThe books, kept by the Payment gateway.\n",
    );
    let edited = engine.scan_all().unwrap();
    assert_eq!(wiki(&edited), 1);
    assert!(edited.iter().any(|r| r.source == TITLE_LINK_SOURCE));
    assert_eq!(mentioned(&engine, "Ledger"), vec!["Payment gateway"]);
}

#[test]
fn a_title_many_pages_name_links_more_weakly() {
    let dir = tempfile::tempdir().unwrap();
    world(dir.path());
    // Fifteen more pages name the Ledger; only the runbook names the Payment gateway.
    for i in 0..15 {
        write(
            &dir.path().join(format!("wiki/entry{i}.md")),
            &format!("# Entry {i}\n\nPosted to the Ledger.\n"),
        );
    }
    let engine = engine(dir.path(), true);
    engine.scan_all().unwrap();
    let pages = engine
        .store()
        .nodes_by_kind(NodeKind::WikiPage, 100, 0)
        .unwrap();
    let id = |name: &str| pages.iter().find(|p| p.name == name).unwrap().id;
    let weight = |from: &str, to: &str| {
        engine
            .store()
            .edges_of(id(from), 100)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EdgeKind::Mentions && e.from == id(from) && e.to == id(to))
            .unwrap()
            .weight
    };
    let specific = weight("Retry runbook", "Payment gateway");
    let common = weight("Retry runbook", "Ledger");
    assert!((specific - 0.75).abs() < 1e-6, "{specific}");
    // Sixteen pages name the Ledger: 0.75 × 10 / 16.
    assert!((common - 0.75 * 10.0 / 16.0).abs() < 1e-3, "{common}");
}
