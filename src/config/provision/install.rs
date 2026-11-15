//! User-space installation of the official Ollama build into the managed root.
//!
//! This replicates the Linux branch of `https://ollama.com/install.sh` without root privileges or
//! a system service: the artifact is `ollama-linux-{amd64,arm64}.tar.zst` (falling back to `.tgz`
//! for releases that predate zstd archives) under `https://ollama.com/download`, an optional
//! `?version=X.Y.Z` pins a release, and the archive unpacks to `bin/ollama` plus `lib/ollama/`.
//! Each release also publishes `sha256sum.txt` (`<hex>  ./<file>` lines), which the script ignores
//! and which is verified here.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::guard::{InstallManifest, ManagedLayout, io_error};
use super::{ProgressFn, ProvisionError, ProvisionEvent, elapsed_ms};
use crate::base::net::http_client_builder;
use crate::base::time::now_ms;

/// Official download location, as used by `install.sh`.
pub const DEFAULT_DOWNLOAD_BASE_URL: &str = "https://ollama.com/download";

const CHECKSUM_FILE: &str = "sha256sum.txt";
/// Archive suffixes in the order `install.sh` tries them.
const ARCHIVE_SUFFIXES: [&str; 2] = [".tar.zst", ".tgz"];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];
/// A stalled transfer is abandoned after this long without a byte.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

/// Base name (without suffix) of the official archive for a platform, e.g. `ollama-linux-amd64`.
///
/// `os` and `arch` use the vocabulary of `std::env::consts`. Anything but Linux on x86_64 or
/// aarch64 is [`ProvisionError::UnsupportedPlatform`].
pub fn artifact_stem(os: &str, arch: &str) -> Result<String, ProvisionError> {
    let arch_name = match (os, arch) {
        ("linux", "x86_64") => "amd64",
        ("linux", "aarch64") => "arm64",
        _ => {
            return Err(ProvisionError::UnsupportedPlatform {
                os: os.to_owned(),
                arch: arch.to_owned(),
            });
        }
    };
    Ok(format!("ollama-linux-{arch_name}"))
}

/// What to install and from where.
pub(super) struct InstallRequest<'a> {
    pub layout: &'a ManagedLayout,
    pub base_url: &'a str,
    pub version: Option<&'a str>,
    pub sha256: Option<&'a str>,
    pub progress: ProgressFn<'a>,
}

/// Result of an installation.
pub(super) struct InstallOutcome {
    pub manifest: InstallManifest,
    /// `false` when the archive came from the download cache.
    pub downloaded: bool,
    pub download_ms: u64,
    pub extract_ms: u64,
}

/// Sidecar describing a cached archive, so that a later run can trust it without the network.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    version: Option<String>,
    sha256: String,
    size: u64,
}

fn download_error(url: &str, reason: impl ToString) -> ProvisionError {
    ProvisionError::Download {
        url: url.to_owned(),
        reason: reason.to_string(),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ProvisionError> + Send + 'static,
) -> Result<T, ProvisionError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| io_error("background task failed", io::Error::other(err)))?
}

fn hash_file(path: &Path) -> Result<(String, u64), ProvisionError> {
    let mut file = std::fs::File::open(path)
        .map_err(|err| io_error(format!("cannot open {}", path.display()), err))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| io_error(format!("cannot read {}", path.display()), err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Finds the checksum of `file` in a `sha256sum`-style listing.
fn parse_checksums(listing: &str, file: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        let name = name.strip_prefix("./").unwrap_or(name);
        (name == file && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn query(version: Option<&str>) -> String {
    version.map(|v| format!("?version={v}")).unwrap_or_default()
}

/// Removes leftovers of interrupted runs: partial downloads and staging directories. Runs under
/// the root lock, so nothing here can belong to a live operation.
fn clean_leftovers(layout: &ManagedLayout) {
    for (dir, is_leftover) in [
        (layout.cache_dir(), (|name: &str| name.ends_with(".part")) as fn(&str) -> bool),
        (layout.root().to_path_buf(), |name: &str| name.starts_with("staging-")),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !is_leftover(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let path = entry.path();
            let removed = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(err) = removed {
                tracing::warn!(path = %path.display(), error = %err, "cannot remove leftover");
            }
        }
    }
}

/// Downloads (or reuses from the cache), verifies and extracts the distribution, replacing any
/// previous one only once the new one is completely on disk.
pub(super) async fn install(request: InstallRequest<'_>) -> Result<InstallOutcome, ProvisionError> {
    let stem = artifact_stem(std::env::consts::OS, std::env::consts::ARCH)?;
    let layout = request.layout.clone();
    let cache = layout.cache_dir();
    {
        let layout = layout.clone();
        blocking(move || {
            std::fs::create_dir_all(layout.cache_dir())
                .map_err(|err| io_error("cannot create the download cache", err))?;
            clean_leftovers(&layout);
            Ok(())
        })
        .await?;
    }

    let http = http_client_builder()
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|err| download_error(request.base_url, err))?;
    let base = request.base_url.trim_end_matches('/');
    let suffix = query(request.version);

    // The published listing; absent for mirrors that do not carry it.
    let listing_url = format!("{base}/{CHECKSUM_FILE}{suffix}");
    let listing = match http.get(&listing_url).timeout(METADATA_TIMEOUT).send().await {
        Ok(response) if response.status().is_success() => response.text().await.ok(),
        _ => None,
    };
    let expected_for = |file: &str| -> Option<String> {
        request
            .sha256
            .map(|hash| hash.trim().to_ascii_lowercase())
            .or_else(|| listing.as_deref().and_then(|text| parse_checksums(text, file)))
    };

    let download_started = Instant::now();
    let mut chosen: Option<(String, CacheMeta, bool, bool)> = None;

    // A cached archive is reused when it is intact and is what would be downloaded now.
    for suffix_name in ARCHIVE_SUFFIXES {
        let file = format!("{stem}{suffix_name}");
        let expected = expected_for(&file);
        let (path, meta_path) = (cache.join(&file), cache.join(format!("{file}.json")));
        let Some(meta) = std::fs::read(&meta_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheMeta>(&bytes).ok())
        else {
            continue;
        };
        if meta.version.as_deref() != request.version
            || expected.as_ref().is_some_and(|hash| *hash != meta.sha256)
        {
            continue;
        }
        let hashed = {
            let path = path.clone();
            blocking(move || hash_file(&path)).await
        };
        if matches!(hashed, Ok((ref hash, size)) if *hash == meta.sha256 && size == meta.size) {
            (request.progress)(ProvisionEvent::Step(format!("using cached {file}")));
            chosen = Some((file, meta, false, expected.is_some()));
            break;
        }
    }

    if chosen.is_none() {
        let mut last_error = None;
        for suffix_name in ARCHIVE_SUFFIXES {
            let file = format!("{stem}{suffix_name}");
            let url = format!("{base}/{file}{suffix}");
            let response = http.get(&url).send().await.map_err(|err| download_error(&url, err))?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                last_error = Some(download_error(&url, "HTTP 404 Not Found"));
                continue;
            }
            if !response.status().is_success() {
                return Err(download_error(&url, format!("HTTP {}", response.status())));
            }
            let expected = expected_for(&file);
            let meta = download(response, &url, &cache, &file, expected.as_deref(), request.progress)
                .await?;
            let meta = CacheMeta {
                version: request.version.map(str::to_owned),
                ..meta
            };
            let encoded = serde_json::to_vec_pretty(&meta)
                .map_err(|err| io_error("cannot encode cache metadata", io::Error::other(err)))?;
            tokio::fs::write(cache.join(format!("{file}.json")), encoded)
                .await
                .map_err(|err| io_error("cannot write cache metadata", err))?;
            chosen = Some((file, meta, true, expected.is_some()));
            break;
        }
        if chosen.is_none() {
            return Err(last_error.unwrap_or_else(|| download_error(base, "no archive found")));
        }
    }
    let (file, meta, downloaded, checksum_verified) =
        chosen.expect("either a cached or a downloaded archive was selected above");
    let download_ms = elapsed_ms(download_started);

    let extract_started = Instant::now();
    (request.progress)(ProvisionEvent::Step(format!("extracting {file}")));
    let archive = cache.join(&file);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let mut task = {
        let (layout, archive) = (layout.clone(), archive.clone());
        tokio::task::spawn_blocking(move || {
            extract_and_swap(&layout, &archive, &|entries| {
                let _ = sender.send(entries);
            })
        })
    };
    let extracted = loop {
        tokio::select! {
            Some(entries) = receiver.recv() => {
                (request.progress)(ProvisionEvent::Extract { entries });
            }
            joined = &mut task => {
                break joined.map_err(|err| io_error("extraction task failed", io::Error::other(err)))?;
            }
        }
    };
    if let Err(err) = extracted {
        if matches!(err, ProvisionError::InvalidArchive { .. } | ProvisionError::UnsafeArchiveEntry { .. }) {
            // A bad archive must not be picked up from the cache by the next attempt.
            let _ = tokio::fs::remove_file(&archive).await;
            let _ = tokio::fs::remove_file(cache.join(format!("{file}.json"))).await;
        }
        return Err(err);
    }

    Ok(InstallOutcome {
        manifest: InstallManifest {
            version: None,
            requested_version: request.version.map(str::to_owned),
            source_url: meta.url,
            archive: file,
            archive_sha256: meta.sha256,
            checksum_verified,
            archive_size: meta.size,
            installed_at: now_ms(),
        },
        downloaded,
        download_ms,
        extract_ms: elapsed_ms(extract_started),
    })
}

/// Streams `response` into `<cache>/<file>.part`, verifies size and checksum, then renames it into
/// place: the cache never holds a file under its final name that is not complete and verified.
async fn download(
    response: reqwest::Response,
    url: &str,
    cache: &Path,
    file: &str,
    expected_sha256: Option<&str>,
    progress: ProgressFn<'_>,
) -> Result<CacheMeta, ProvisionError> {
    let total = response.content_length();
    let partial = cache.join(format!("{file}.part"));
    let write_error = |err| io_error(format!("cannot write {}", partial.display()), err);
    let mut output = tokio::fs::File::create(&partial).await.map_err(write_error)?;
    progress(ProvisionEvent::Download {
        file: file.to_owned(),
        received: 0,
        total,
    });

    let result: Result<u64, ProvisionError> = async {
        let mut stream = response.bytes_stream();
        let (mut received, mut reported) = (0u64, 0u64);
        // About a hundred progress events per download, however large.
        let step = total.map_or(4 << 20, |total| (total / 100).max(64 << 10));
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| download_error(url, err))?;
            output.write_all(&chunk).await.map_err(write_error)?;
            received += chunk.len() as u64;
            if received - reported >= step {
                reported = received;
                progress(ProvisionEvent::Download {
                    file: file.to_owned(),
                    received,
                    total,
                });
            }
        }
        output.flush().await.map_err(write_error)?;
        output.sync_all().await.map_err(write_error)?;
        if let Some(total) = total
            && total != received
        {
            return Err(download_error(
                url,
                format!("incomplete download: received {received} of {total} bytes"),
            ));
        }
        Ok(received)
    }
    .await;
    drop(output);

    let verified = match result {
        Ok(received) => {
            let path = partial.clone();
            blocking(move || hash_file(&path)).await.and_then(|(actual, size)| {
                match expected_sha256 {
                    Some(expected) if expected != actual => Err(ProvisionError::ChecksumMismatch {
                        file: file.to_owned(),
                        expected: expected.to_owned(),
                        actual,
                    }),
                    _ if size != received => Err(download_error(url, "file changed while hashing")),
                    _ => Ok((actual, size)),
                }
            })
        }
        Err(err) => Err(err),
    };
    let (sha256, size) = match verified {
        Ok(verified) => verified,
        Err(err) => {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(err);
        }
    };
    tokio::fs::rename(&partial, cache.join(file))
        .await
        .map_err(|err| io_error("cannot move the download into the cache", err))?;
    progress(ProvisionEvent::Download {
        file: file.to_owned(),
        received: size,
        total: Some(size),
    });
    Ok(CacheMeta {
        url: url.to_owned(),
        version: None,
        sha256,
        size,
    })
}

fn unsafe_entry(entry: &Path, reason: &str) -> ProvisionError {
    ProvisionError::UnsafeArchiveEntry {
        entry: entry.to_string_lossy().into_owned(),
        reason: reason.to_owned(),
    }
}

/// Depth of `path` below the extraction root after lexical normalisation; `None` when the path is
/// absolute or climbs above the root at any point.
fn contained_depth(start: usize, path: &Path) -> Option<usize> {
    let mut depth = start;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => depth = depth.checked_sub(1)?,
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(depth)
}

/// Extracts `archive` into a staging directory and renames it over `dist/` once complete.
fn extract_and_swap(
    layout: &ManagedLayout,
    archive: &Path,
    on_entry: &dyn Fn(u64),
) -> Result<(), ProvisionError> {
    let unique = format!("{}-{}", std::process::id(), now_ms());
    let staging = layout.root().join(format!("staging-{unique}"));
    let result = extract(archive, &staging, on_entry).and_then(|()| {
        let binary = staging.join("bin").join("ollama");
        if !std::fs::metadata(&binary).is_ok_and(|meta| meta.is_file()) {
            return Err(ProvisionError::InvalidArchive {
                archive: archive.to_path_buf(),
                reason: "it does not contain bin/ollama".to_owned(),
            });
        }
        let dist = layout.dist_dir();
        let retired = layout.root().join(format!("staging-old-{unique}"));
        let had_previous = match std::fs::rename(&dist, &retired) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(err) => return Err(io_error("cannot retire the previous distribution", err)),
        };
        std::fs::rename(&staging, &dist)
            .map_err(|err| io_error("cannot activate the new distribution", err))?;
        if had_previous {
            let _ = std::fs::remove_dir_all(&retired);
        }
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn extract(archive: &Path, staging: &Path, on_entry: &dyn Fn(u64)) -> Result<(), ProvisionError> {
    let invalid = |reason: String| ProvisionError::InvalidArchive {
        archive: archive.to_path_buf(),
        reason,
    };
    let mut file = std::fs::File::open(archive)
        .map_err(|err| io_error(format!("cannot open {}", archive.display()), err))?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).map_err(|err| invalid(err.to_string()))?;
    let file = std::fs::File::open(archive)
        .map_err(|err| io_error(format!("cannot open {}", archive.display()), err))?;
    // Decided by content, not by name: mirrors rename files.
    let reader: Box<dyn Read> = if read == 4 && magic == ZSTD_MAGIC {
        let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|err| invalid(err.to_string()))?;
        // Official archives are compressed with a long window, beyond the decoder's default limit.
        decoder.window_log_max(31).map_err(|err| invalid(err.to_string()))?;
        Box::new(decoder)
    } else if read >= 2 && magic[..2] == GZIP_MAGIC {
        Box::new(flate2::read::GzDecoder::new(io::BufReader::new(file)))
    } else {
        return Err(invalid("neither a zstd nor a gzip archive".to_owned()));
    };

    std::fs::create_dir_all(staging)
        .map_err(|err| io_error(format!("cannot create {}", staging.display()), err))?;
    let mut tar = tar::Archive::new(reader);
    let mut count = 0u64;
    for entry in tar.entries().map_err(|err| invalid(err.to_string()))? {
        let mut entry = entry.map_err(|err| invalid(err.to_string()))?;
        let path = entry.path().map_err(|err| invalid(err.to_string()))?.into_owned();
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let Some(depth) = contained_depth(0, &path) else {
            return Err(unsafe_entry(&path, "absolute or parent-escaping path"));
        };
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|err| invalid(err.to_string()))?
                .ok_or_else(|| unsafe_entry(&path, "link without a target"))?;
            // Symlink targets are relative to the link's directory, hard-link targets to the
            // archive root.
            let start = if kind.is_symlink() { depth.saturating_sub(1) } else { 0 };
            if contained_depth(start, &target).is_none() {
                return Err(unsafe_entry(&path, "link pointing outside the archive"));
            }
        } else if !(kind.is_file() || kind.is_dir()) {
            return Err(unsafe_entry(&path, "unsupported entry type (device, fifo, ...)"));
        }
        // `unpack_in` re-checks containment on the real filesystem (symlinked parents) and keeps
        // the executable bits of the entry's mode.
        let unpacked = entry.unpack_in(staging).map_err(|err| invalid(format!("{}: {err}", path.display())))?;
        if !unpacked {
            return Err(unsafe_entry(&path, "entry would be written outside the destination"));
        }
        count += 1;
        if count.is_multiple_of(100) {
            on_entry(count);
        }
    }
    if count == 0 {
        return Err(invalid("the archive is empty".to_owned()));
    }
    on_entry(count);
    Ok(())
}

/// Location of the staged archive for tests and diagnostics.
#[allow(dead_code)]
pub(super) fn cached_archive(layout: &ManagedLayout, file: &str) -> PathBuf {
    layout.cache_dir().join(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names_follow_install_sh() {
        assert_eq!(artifact_stem("linux", "x86_64").expect("amd64"), "ollama-linux-amd64");
        assert_eq!(artifact_stem("linux", "aarch64").expect("arm64"), "ollama-linux-arm64");
        for (os, arch) in [("macos", "aarch64"), ("windows", "x86_64"), ("linux", "riscv64")] {
            let err = artifact_stem(os, arch).expect_err("unsupported");
            assert!(err.to_string().contains("ollama.base_url"), "{err}");
        }
        assert_eq!(query(Some("0.5.7")), "?version=0.5.7");
        assert_eq!(query(None), "");
    }

    #[test]
    fn checksum_listing_is_parsed() {
        let hash = "87bfafb2acbc89b2e6a1be0a9043de6aa75fb450696f86bc426e65fc448f788d";
        let listing = format!("abc  ./short\n{hash}  ./ollama-linux-amd64.tar.zst\n{hash} *plain.tgz\n");
        assert_eq!(parse_checksums(&listing, "ollama-linux-amd64.tar.zst").as_deref(), Some(hash));
        assert_eq!(parse_checksums(&listing, "plain.tgz").as_deref(), Some(hash));
        assert_eq!(parse_checksums(&listing, "short"), None);
        assert_eq!(parse_checksums(&listing, "ollama-linux-arm64.tar.zst"), None);
    }

    #[test]
    fn containment_is_lexical_and_strict() {
        assert_eq!(contained_depth(0, Path::new("bin/ollama")), Some(2));
        assert_eq!(contained_depth(0, Path::new("./lib/../bin")), Some(1));
        assert_eq!(contained_depth(0, Path::new("../etc/passwd")), None);
        assert_eq!(contained_depth(0, Path::new("a/../../b")), None);
        assert_eq!(contained_depth(0, Path::new("/etc/passwd")), None);
        // A symlink at lib/ollama/x -> ../../bin/ollama stays inside; three levels up does not.
        assert_eq!(contained_depth(2, Path::new("../../bin/ollama")), Some(2));
        assert_eq!(contained_depth(2, Path::new("../../../x")), None);
    }
}
