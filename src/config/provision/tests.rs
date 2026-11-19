//! Hermetic tests of the provisioning engine (fake Ollama, fake launcher, local download server),
//! plus the opt-in real end-to-end test.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::testing::*;
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn entries(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn fresh_provision_installs_starts_pulls_and_verifies() -> TestResult {
    let rig = TestRig::new().await?;
    let events = Mutex::new(Vec::new());
    let report = rig
        .manager
        .ensure_ready(&rig.options, &|event| events.lock().unwrap().push(event))
        .await?;

    assert_eq!(report.mode, ServerMode::ManagedStarted);
    assert!(report.installed && report.downloaded && report.started && report.pulled);
    assert_eq!(report.binary_source, Some(BinarySource::Managed));
    assert_eq!(report.version.as_deref(), Some("0.0.0-fake"));
    let verification = report.verification.as_ref().expect("verified");
    assert_eq!(
        (verification.prompt_tokens, verification.completion_tokens),
        (7, 2)
    );
    for step in ["probe", "install", "start", "model", "verify"] {
        assert!(
            report.steps.iter().any(|s| s.step == step),
            "missing step {step}"
        );
    }

    let layout = rig.manager.layout();
    assert!(layout.is_marked());
    let binary = std::fs::metadata(layout.binary())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            binary.permissions().mode() & 0o111,
            0o111,
            "executable bit preserved"
        );
    }
    assert!(binary.is_file());
    let spec = &rig.launcher.specs()[0];
    assert_eq!(spec.binary, layout.binary());
    assert_eq!(spec.models_dir, layout.models_dir());
    assert_eq!(spec.host, "127.0.0.1");
    assert_eq!(report.base_url, format!("http://127.0.0.1:{}", spec.port));

    let (state, _) = ManagedState::load(layout);
    let install = state.install.expect("manifest");
    assert!(install.checksum_verified);
    assert_eq!(install.version.as_deref(), Some("0.0.0-fake"));
    assert_eq!(state.server.expect("server").port, spec.port);
    assert_eq!(state.models.len(), 1);
    assert!(state.models[0].managed);

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Download { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Extract { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        ProvisionEvent::Pull {
            completed: Some(500),
            ..
        }
    )));
    Ok(())
}

#[tokio::test]
async fn ensure_ready_is_idempotent() -> TestResult {
    let rig = TestRig::new().await?;
    let first = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let hits = rig.downloads.hits();
    let second = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(second.mode, ServerMode::ManagedRunning);
    assert_eq!(second.base_url, first.base_url);
    assert!(!second.installed && !second.started && !second.pulled);
    assert_eq!(rig.launcher.specs().len(), 1);
    assert_eq!(rig.downloads.hits(), hits);
    Ok(())
}

#[tokio::test]
async fn concurrent_calls_are_serialised_by_the_lock() -> TestResult {
    let rig = TestRig::new().await?;
    let (a, b) = tokio::join!(
        rig.manager.ensure_ready(&rig.options, &no_progress),
        rig.manager.ensure_ready(&rig.options, &no_progress)
    );
    let (a, b) = (a?, b?);
    assert_eq!(
        rig.launcher.specs().len(),
        1,
        "exactly one server was started"
    );
    assert_eq!([a.started, b.started].iter().filter(|s| **s).count(), 1);
    Ok(())
}

#[tokio::test]
async fn external_endpoint_is_used_never_installed_or_stopped() -> TestResult {
    let mut rig = TestRig::new().await?;
    let external = FakeOllama::start(0).await?;
    rig.config.base_url = external.base_url();
    rig.manager = OllamaManager::with_parts(
        rig.config.clone(),
        rig.root().to_path_buf(),
        rig.launcher.clone(),
        Arc::new(FixedLocator(None)),
    )
    .with_home(None);

    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(report.mode, ServerMode::External);
    assert_eq!(report.base_url, external.base_url());
    assert!(report.pulled && !report.installed && !report.started);
    assert_eq!(*external.state().pulls.lock().unwrap(), ["tiny:latest"]);
    assert!(rig.launcher.specs().is_empty());
    assert_eq!(rig.downloads.hits(), 0);

    // Teardown does not stop it, and does not touch its models unless asked to.
    let teardown = rig.manager.teardown(&TeardownOptions::default()).await?;
    assert!(!teardown.stopped_server);
    assert!(external.state().deletes.lock().unwrap().is_empty());
    let api = OllamaApi::new(&external.base_url())?;
    assert!(api.is_reachable().await);

    Ok(())
}

#[tokio::test]
async fn external_models_are_deleted_only_on_request() -> TestResult {
    let mut rig = TestRig::new().await?;
    let external = FakeOllama::start_with(0, &["precious:1b"], None).await?;
    rig.config.base_url = external.base_url();
    rig.manager = OllamaManager::with_parts(
        rig.config.clone(),
        rig.root().to_path_buf(),
        rig.launcher.clone(),
        Arc::new(FixedLocator(None)),
    )
    .with_home(None);
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let keep = TeardownOptions {
        keep_distribution: true,
        ..TeardownOptions::default()
    };
    rig.manager.teardown(&keep).await?;
    assert!(external.state().deletes.lock().unwrap().is_empty());

    rig.manager.ensure_ready(&rig.options, &no_progress).await?; // already present: not recorded
    external
        .state()
        .models
        .lock()
        .unwrap()
        .retain(|m| m != "tiny:latest");
    rig.manager.ensure_ready(&rig.options, &no_progress).await?; // pulled again and recorded
    let teardown = rig
        .manager
        .teardown(&TeardownOptions {
            remove_external_models: true,
            ..TeardownOptions::default()
        })
        .await?;
    assert_eq!(teardown.external_models_deleted, ["tiny:latest"]);
    assert_eq!(*external.state().deletes.lock().unwrap(), ["tiny:latest"]);
    assert_eq!(*external.state().models.lock().unwrap(), ["precious:1b"]);
    Ok(())
}

#[tokio::test]
async fn a_system_binary_is_preferred_over_downloading() -> TestResult {
    let rig = TestRig::with_system_binary("/usr/local/bin/ollama".into()).await?;
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert_eq!(report.binary_source, Some(BinarySource::System));
    assert!(!report.installed);
    assert_eq!(rig.downloads.hits(), 0);
    assert_eq!(
        rig.launcher.specs()[0].binary,
        Path::new("/usr/local/bin/ollama")
    );
    assert!(!rig.manager.layout().dist_dir().exists());

    // Teardown never touches the system binary and leaves no install behind.
    let teardown = rig.manager.teardown(&TeardownOptions::default()).await?;
    assert!(teardown.stopped_server);
    assert!(teardown.root_removed);
    Ok(())
}

#[tokio::test]
async fn a_taken_port_falls_back_to_another() -> TestResult {
    let rig = TestRig::new().await?;
    let preferred = rig.options.preferred_port.expect("port");
    let squatter = std::net::TcpListener::bind(("127.0.0.1", preferred))?;
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let port = rig.launcher.specs()[0].port;
    assert_ne!(port, preferred);
    assert_eq!(report.base_url, format!("http://127.0.0.1:{port}"));
    drop(squatter);
    Ok(())
}

#[tokio::test]
async fn an_unhealthy_server_times_out_and_is_cleaned_up() -> TestResult {
    let rig = TestRig::new().await?;
    *rig.launcher.never_healthy.lock().unwrap() = true;
    let options = ProvisionOptions {
        startup_timeout: Some(Duration::from_millis(700)),
        ..rig.options.clone()
    };
    let err = rig
        .manager
        .ensure_ready(&options, &no_progress)
        .await
        .expect_err("timeout");
    assert!(matches!(err, ProvisionError::HealthTimeout { .. }), "{err}");
    assert_eq!(
        rig.launcher.running(),
        0,
        "the half-started process was stopped"
    );
    let (state, _) = ManagedState::load(rig.manager.layout());
    assert!(state.server.is_none());

    // A process that dies immediately is reported as such, not as a timeout.
    *rig.launcher.never_healthy.lock().unwrap() = false;
    *rig.launcher.fail_spawn.lock().unwrap() = Some("exec format error".to_owned());
    let err = rig
        .manager
        .ensure_ready(&rig.options, &no_progress)
        .await
        .expect_err("spawn");
    assert!(err.to_string().contains("exec format error"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_failed_pull_surfaces_the_remote_error() -> TestResult {
    let rig = TestRig::new().await?;
    *rig.launcher.pull_error.lock().unwrap() =
        Some("pull model manifest: file does not exist".to_owned());
    let err = rig
        .manager
        .ensure_ready(&rig.options, &no_progress)
        .await
        .expect_err("pull");
    let message = err.to_string();
    assert!(
        message.contains("tiny:latest") && message.contains("file does not exist"),
        "{message}"
    );
    Ok(())
}

async fn rig_with_archive(archive: Vec<u8>, listing: Option<String>) -> std::io::Result<TestRig> {
    let mut files = BTreeMap::from([(archive_name(), archive)]);
    if let Some(listing) = listing {
        files.insert("sha256sum.txt".to_owned(), listing.into_bytes());
    }
    TestRig::with_downloads(FakeDownloadServer::start(files).await?)
}

#[tokio::test]
async fn a_checksum_mismatch_is_rejected_and_leaves_nothing() -> TestResult {
    let archive = distribution_tar_zst();
    let wrong = format!("{}  ./{}\n", "0".repeat(64), archive_name());
    let rig = rig_with_archive(archive, Some(wrong)).await?;
    let err = rig
        .manager
        .ensure_ready(&rig.options, &no_progress)
        .await
        .expect_err("checksum");
    assert!(
        matches!(err, ProvisionError::ChecksumMismatch { .. }),
        "{err}"
    );
    let layout = rig.manager.layout();
    assert!(!layout.dist_dir().exists());
    assert!(
        entries(&layout.cache_dir()).is_empty(),
        "no partial or cached file: {:?}",
        entries(&layout.cache_dir())
    );
    assert_eq!(rig.launcher.running(), 0);

    // A caller-supplied checksum overrides the published one.
    let archive = distribution_tar_zst();
    let rig = rig_with_archive(
        archive.clone(),
        Some(format!("{}  ./{}\n", "1".repeat(64), archive_name())),
    )
    .await?;
    let options = ProvisionOptions {
        sha256: Some(sha256_hex(&archive)),
        ..rig.options.clone()
    };
    rig.manager.ensure_ready(&options, &no_progress).await?;
    Ok(())
}

#[tokio::test]
async fn corrupt_and_hostile_archives_leave_no_half_install() -> TestResult {
    let mut corrupt = vec![0x28, 0xB5, 0x2F, 0xFD];
    corrupt.extend_from_slice(b"definitely not a zstd frame");
    let traversal = zstd_compress(&tar_of(&[("bin/ollama", b"x"), ("../escaped", b"pwned")]));
    let absolute = zstd_compress(&tar_of(&[("/tmp/anchor-escaped", b"pwned")]));
    let no_binary = zstd_compress(&tar_of(&[("readme", b"hi")]));
    let unknown = b"plain text, no compression".to_vec();

    for (label, archive) in [
        ("corrupt", corrupt),
        ("traversal", traversal),
        ("absolute", absolute),
        ("no binary", no_binary),
        ("unknown format", unknown),
    ] {
        let rig = rig_with_archive(archive, None).await?;
        let err = rig
            .manager
            .ensure_ready(&rig.options, &no_progress)
            .await
            .expect_err(label);
        assert!(
            matches!(
                err,
                ProvisionError::InvalidArchive { .. } | ProvisionError::UnsafeArchiveEntry { .. }
            ),
            "{label}: {err}"
        );
        let layout = rig.manager.layout();
        assert!(!layout.dist_dir().exists(), "{label}: no dist");
        assert!(
            !entries(layout.root())
                .iter()
                .any(|n| n.starts_with("staging-")),
            "{label}: staging removed"
        );
        assert!(
            entries(&layout.cache_dir()).is_empty(),
            "{label}: bad archive not cached"
        );
        assert!(
            !layout
                .root()
                .parent()
                .expect("parent")
                .join("escaped")
                .exists()
        );
        assert!(!Path::new("/tmp/anchor-escaped").exists());
        assert_eq!(rig.launcher.running(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn gzip_archives_are_accepted_too() -> TestResult {
    let archive = gzip_compress(&distribution_tar());
    let rig = rig_with_archive(archive, None).await?;
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert!(report.installed);
    let (state, _) = ManagedState::load(rig.manager.layout());
    assert!(
        !state.install.expect("manifest").checksum_verified,
        "no checksum was published"
    );
    Ok(())
}

#[tokio::test]
async fn teardown_removes_exactly_the_managed_files() -> TestResult {
    let rig = TestRig::new().await?;
    let outside = rig.dir.path().join("data").join("anchor.db");
    std::fs::create_dir_all(outside.parent().expect("parent"))?;
    std::fs::write(&outside, "database")?;
    let user_ollama = rig.dir.path().join("home").join(".ollama");
    std::fs::create_dir_all(&user_ollama)?;
    std::fs::write(user_ollama.join("id_ed25519"), "key")?;

    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let teardown = rig.manager.teardown(&TeardownOptions::default()).await?;
    assert!(teardown.stopped_server && teardown.root_removed);
    assert!(
        teardown.bytes_freed > 2048,
        "bytes: {}",
        teardown.bytes_freed
    );
    assert!(
        teardown
            .removed
            .iter()
            .all(|r| r.path.starts_with(rig.root().parent().expect("p")))
    );
    assert!(!rig.root().exists());
    assert_eq!(rig.launcher.running(), 0);
    assert_eq!(std::fs::read_to_string(&outside)?, "database");
    assert!(user_ollama.join("id_ed25519").exists());

    // A second teardown is a no-op.
    assert_eq!(
        rig.manager.teardown(&TeardownOptions::default()).await?,
        TeardownReport::default()
    );
    Ok(())
}

#[tokio::test]
async fn teardown_keeps_what_it_is_told_to_and_unknown_files() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    std::fs::write(rig.root().join("notes.txt"), "mine")?;
    let teardown = rig
        .manager
        .teardown(&TeardownOptions {
            keep_distribution: true,
            keep_cache: true,
            ..TeardownOptions::default()
        })
        .await?;
    assert!(!teardown.root_removed);
    let layout = rig.manager.layout();
    assert!(layout.binary().is_file() && layout.cache_dir().is_dir());
    assert!(!layout.models_dir().exists() && !layout.log_file().exists());
    assert!(layout.is_marked());
    assert!(rig.root().join("notes.txt").exists());

    // Wiping everything leaves the unknown file (and the marker), not a half-deleted root.
    rig.manager.teardown(&TeardownOptions::default()).await?;
    assert!(rig.root().join("notes.txt").exists());
    assert!(layout.is_marked());
    assert!(!layout.dist_dir().exists());
    Ok(())
}

#[tokio::test]
async fn teardown_refuses_unmarked_and_dangerous_roots() -> TestResult {
    let rig = TestRig::new().await?;
    let precious = rig.dir.path().join("precious");
    std::fs::create_dir_all(&precious)?;
    std::fs::write(precious.join("file"), "keep")?;
    let config = rig.config.clone();
    let manager = |root: &Path, home: Option<&Path>| {
        OllamaManager::with_parts(
            config.clone(),
            root,
            rig.launcher.clone(),
            Arc::new(FixedLocator(None)),
        )
        .with_home(home.map(Path::to_path_buf))
    };

    let err = manager(&precious, None)
        .teardown(&TeardownOptions::default())
        .await
        .expect_err("unmarked");
    assert!(matches!(err, ProvisionError::UnsafePath { .. }), "{err}");
    assert!(precious.join("file").exists());
    for dangerous in ["/", "/tmp", "/usr"] {
        let err = manager(Path::new(dangerous), None)
            .teardown(&TeardownOptions::default())
            .await
            .expect_err(dangerous);
        assert!(
            matches!(err, ProvisionError::UnsafePath { .. }),
            "{dangerous}: {err}"
        );
    }
    // Provisioning also refuses to adopt a non-empty foreign directory.
    let err = manager(&precious, None)
        .ensure_ready(&rig.options, &no_progress)
        .await
        .expect_err("adopt");
    assert!(matches!(err, ProvisionError::UnsafePath { .. }), "{err}");
    assert_eq!(entries(&precious), ["file"]);
    Ok(())
}

#[tokio::test]
async fn stale_state_and_dead_pids_are_recovered() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    rig.launcher.crash_all();
    let status = rig.manager.status().await;
    assert!(status.server.is_some() && !status.server_alive);
    assert!(status.distribution_present && !status.endpoint.reachable());

    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert!(
        report.started && !report.installed,
        "restarted from the existing installation"
    );
    assert_eq!(rig.launcher.specs().len(), 2);

    // A corrupt state file is ignored and rewritten; the installation is then reinstalled.
    rig.manager.stop(Duration::from_secs(1)).await?;
    std::fs::write(rig.manager.layout().state_file(), "{ broken")?;
    assert!(rig.manager.status().await.state_recovered);
    let report = rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    assert!(report.started && report.installed);
    Ok(())
}

#[tokio::test]
async fn status_reports_endpoint_install_and_system_binary() -> TestResult {
    let rig = TestRig::with_system_binary("/opt/ollama".into()).await?;
    let before = rig.manager.status().await;
    assert!(!before.endpoint.reachable() && !before.root_marked && before.install.is_none());
    assert_eq!(
        before.system_binary.as_deref(),
        Some(Path::new("/opt/ollama"))
    );
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let after = rig.manager.status().await;
    assert!(after.server_alive && after.root_marked);
    assert!(
        after
            .server_endpoint
            .expect("endpoint")
            .models
            .iter()
            .any(|m| m == "tiny:latest")
    );
    assert_eq!(after.pulled_models.len(), 1);
    Ok(())
}

#[tokio::test]
async fn rebuild_wipes_and_reprovisions_with_timings() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let hits = rig.downloads.hits();

    // Keeping the cache: nothing is downloaded again, the distribution is reinstalled from it.
    let report = rig
        .manager
        .rebuild(
            &RebuildOptions {
                teardown: TeardownOptions {
                    keep_cache: true,
                    ..TeardownOptions::default()
                },
                provision: rig.options.clone(),
            },
            &no_progress,
        )
        .await?;
    assert!(report.teardown.stopped_server && !report.teardown.root_removed);
    assert!(report.provision.installed && !report.provision.downloaded);
    assert!(report.provision.started && report.provision.pulled);
    assert_eq!(report.verification.completion_tokens, 2);
    assert_eq!(
        rig.downloads.hits(),
        hits + 1,
        "only the checksum listing was fetched"
    );
    assert!(report.total_ms >= report.teardown_ms.max(report.provision_ms));
    assert_eq!(rig.launcher.running(), 1);

    // Full wipe: downloads again.
    let report = rig
        .manager
        .rebuild(
            &RebuildOptions {
                provision: rig.options.clone(),
                ..RebuildOptions::default()
            },
            &no_progress,
        )
        .await?;
    assert!(report.provision.downloaded);

    // Keeping the distribution: no install at all.
    let report = rig
        .manager
        .rebuild(
            &RebuildOptions {
                teardown: TeardownOptions {
                    keep_distribution: true,
                    ..TeardownOptions::default()
                },
                provision: rig.options.clone(),
            },
            &no_progress,
        )
        .await?;
    assert!(!report.provision.installed && report.provision.started);
    Ok(())
}

#[tokio::test]
async fn verify_reports_latency_and_tokens_and_failures() -> TestResult {
    let rig = TestRig::new().await?;
    rig.manager.ensure_ready(&rig.options, &no_progress).await?;
    let report = rig.manager.verify("tiny:latest").await?;
    assert_eq!((report.prompt_tokens, report.completion_tokens), (7, 2));
    assert_eq!(report.response, "ready");
    let err = rig.manager.verify("missing:1b").await.expect_err("missing");
    assert!(matches!(err, ProvisionError::VerifyFailed { .. }), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_embedding_model_is_verified_by_embedding() -> TestResult {
    let rig = TestRig::new().await?;
    let options = ProvisionOptions {
        model: Some("nomic-embed-text".to_owned()),
        ..rig.options.clone()
    };
    let report = rig.manager.ensure_ready(&options, &no_progress).await?;
    let verified = report.verification.expect("verified");
    assert_eq!(verified.response, "embedding of 4 dimensions");
    Ok(())
}

/// Real end-to-end test against the official Ollama build. Run with
/// `ANCHOR_E2E_OLLAMA=1 cargo test --all-features e2e_real_ollama -- --ignored --nocapture`.
/// Environment: `ANCHOR_E2E_OLLAMA_MODEL` (default `smollm2:135m`), `ANCHOR_E2E_OLLAMA_PORT`.
#[tokio::test]
#[ignore = "downloads the real Ollama build; set ANCHOR_E2E_OLLAMA=1 and pass --ignored"]
async fn e2e_real_ollama() -> TestResult {
    if std::env::var("ANCHOR_E2E_OLLAMA").as_deref() != Ok("1") {
        eprintln!("skipped: ANCHOR_E2E_OLLAMA=1 is not set");
        return Ok(());
    }
    let model =
        std::env::var("ANCHOR_E2E_OLLAMA_MODEL").unwrap_or_else(|_| "smollm2:135m".to_owned());
    let port: u16 = std::env::var("ANCHOR_E2E_OLLAMA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(21434);
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tmp");
    std::fs::create_dir_all(&base)?;
    let work = tempfile::Builder::new()
        .prefix("e2e-ollama-")
        .tempdir_in(&base)?;
    let root = work.path().join("ollama");

    let config = OllamaConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        default_model: model.clone(),
        startup_timeout_secs: 120,
        ..OllamaConfig::default()
    };
    // The host may well have a system ollama: the point is to exercise the download path.
    let manager = OllamaManager::with_root(config, &root).with_home(None);
    let options = ProvisionOptions {
        use_system_binary: false,
        preferred_port: Some(port),
        startup_timeout: Some(Duration::from_secs(120)),
        verify_timeout: Duration::from_secs(600),
        ..ProvisionOptions::default()
    };
    let last_print = Mutex::new(Instant::now());
    let progress = |event: ProvisionEvent| {
        let mut last = last_print.lock().unwrap();
        match &event {
            ProvisionEvent::Step(text) => eprintln!("[e2e] {text}"),
            _ if last.elapsed() > Duration::from_secs(5) => {
                *last = Instant::now();
                eprintln!("[e2e] {event:?}");
            }
            _ => {}
        }
    };

    let started = Instant::now();
    let first = manager.ensure_ready(&options, &progress).await?;
    eprintln!(
        "[e2e] first provision: {} ms {:?}",
        first.total_ms, first.steps
    );
    assert!(first.installed && first.downloaded && first.started && first.pulled);
    let verification = first.verification.as_ref().expect("verified");
    assert!(verification.completion_tokens > 0 || !verification.response.is_empty());
    eprintln!("[e2e] verify: {verification:?}");

    let teardown = manager
        .teardown(&TeardownOptions {
            keep_cache: true,
            ..TeardownOptions::default()
        })
        .await?;
    eprintln!(
        "[e2e] teardown (cache kept): freed {} bytes",
        teardown.bytes_freed
    );
    assert!(teardown.stopped_server);
    assert!(
        !OllamaApi::new(&first.base_url)?
            .with_probe_timeout(Duration::from_secs(1))
            .is_reachable()
            .await
    );

    let rebuilt = manager
        .rebuild(
            &RebuildOptions {
                teardown: TeardownOptions {
                    keep_cache: true,
                    ..TeardownOptions::default()
                },
                provision: options.clone(),
            },
            &progress,
        )
        .await?;
    eprintln!(
        "[e2e] rebuild: teardown {} ms, provision {} ms (downloaded again: {}), verify {} ms, total {} ms",
        rebuilt.teardown_ms,
        rebuilt.provision_ms,
        rebuilt.provision.downloaded,
        rebuilt.verify_ms,
        rebuilt.total_ms
    );
    assert!(
        !rebuilt.provision.downloaded,
        "the cached archive was reused"
    );
    assert!(
        rebuilt.provision.pulled,
        "models were wiped, so the model was pulled again"
    );

    let teardown = manager.teardown(&TeardownOptions::default()).await?;
    eprintln!("[e2e] final teardown freed {} bytes", teardown.bytes_freed);
    assert!(
        teardown.root_removed && !root.exists(),
        "directory is clean"
    );
    assert!(
        !OllamaApi::new(&rebuilt.provision.base_url)?
            .with_probe_timeout(Duration::from_secs(1))
            .is_reachable()
            .await
    );
    eprintln!("[e2e] total {:?}", started.elapsed());
    Ok(())
}
