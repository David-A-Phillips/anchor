//! Ingestion of hostile or malformed repository content: nothing in a scanned repository may
//! silently drop what the rest of the repository says, or abort the scan.
#![allow(clippy::unwrap_used)]

use anchor::base::{NodeKind, SourceKind};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::router::ProviderKind;

fn engine_over(repo: &std::path::Path, data: &std::path::Path) -> std::sync::Arc<Engine> {
    let mut config = AnchorConfig {
        data_dir: Some(data.to_path_buf()),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config
        .sources
        .push(SourceConfig::new("svc", SourceKind::Repo, repo.to_string_lossy()));
    Engine::open(config).unwrap()
}

#[test]
fn an_ownership_file_that_is_not_valid_utf8_still_yields_its_owners() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("svc");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    // A legacy-encoded comment (latin-1 `é`) before the rule.
    std::fs::write(
        repo.join("CODEOWNERS"),
        b"# \xc9quipe paiements (caf\xe9)\n* @acme/payments-platform\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let engine = engine_over(&repo, &dir.path().join("data"));
    let reports = engine.scan_all().unwrap();
    assert!(
        reports[0].warnings.iter().all(|w| !w.contains("cannot read")),
        "{:?}",
        reports[0].warnings
    );
    let teams = engine.store().nodes_by_kind(NodeKind::Team, 10, 0).unwrap();
    assert!(
        teams.iter().any(|t| t.key.as_str() == "team:payments-platform"),
        "the owner survives the bad bytes: {:?}",
        teams.iter().map(|t| t.key.to_string()).collect::<Vec<_>>()
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_ownership_file_is_reported_instead_of_silently_dropping_ownership() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("svc");
    std::fs::create_dir_all(&repo).unwrap();
    let codeowners = repo.join("CODEOWNERS");
    std::fs::write(&codeowners, "* @acme/payments-platform\n").unwrap();
    std::fs::set_permissions(&codeowners, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&codeowners).is_ok() {
        // Running as root: permissions do not apply, so there is nothing to observe.
        return;
    }
    let engine = engine_over(&repo, &dir.path().join("data"));
    let reports = engine.scan_all().unwrap();
    assert!(
        reports[0]
            .warnings
            .iter()
            .any(|w| w.contains("CODEOWNERS") && w.contains("cannot read")),
        "{:?}",
        reports[0].warnings
    );
}
