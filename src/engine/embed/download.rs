//! Locating and downloading sentence-embedding model files.
//!
//! Nothing here runs implicitly: [`ensure_model`] is only called when the user asks for a model
//! download (setup wizard, CLI).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::base::{Error, Result, net};

/// Files that make up a model directory.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

const HUGGING_FACE: &str = "https://huggingface.co";
/// A stalled transfer is abandoned after this long without receiving a byte.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Sanity ceilings: a response larger than this is not the file we asked for.
const MAX_JSON_BYTES: u64 = 64 << 20;
const MAX_WEIGHTS_BYTES: u64 = 8 << 30;

/// Progress of [`ensure_model`], reported after every received block and once per finished file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    /// File being downloaded (one of [`MODEL_FILES`]).
    pub file: &'static str,
    /// Bytes received so far for this file.
    pub downloaded: u64,
    /// Size announced by the server, when it announced one.
    pub total: Option<u64>,
}

/// Directory holding the files of `repo_id` (e.g. `sentence-transformers/all-MiniLM-L6-v2`) under
/// `models_dir`: the repository id with `/` replaced by `--`.
///
/// Fails with [`Error::Config`] for ids that are not `name` or `owner/name` made of letters,
/// digits, `-`, `_` and `.`; this also keeps the path inside `models_dir`.
pub fn model_dir(models_dir: &Path, repo_id: &str) -> Result<PathBuf> {
    let segments: Vec<&str> = repo_id.split('/').collect();
    let valid = (1..=2).contains(&segments.len())
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if !valid {
        return Err(Error::config(format!(
            "invalid embedding model id {repo_id:?}: expected a Hugging Face repository such as \
             sentence-transformers/all-MiniLM-L6-v2"
        )));
    }
    Ok(models_dir.join(segments.join("--")))
}

/// `true` when every file of [`MODEL_FILES`] exists in `dir` (content is not inspected).
pub fn model_files_present(dir: &Path) -> bool {
    MODEL_FILES.iter().all(|file| dir.join(file).is_file())
}

/// Downloads the files of `repo_id` from Hugging Face into [`model_dir`] and returns that
/// directory.
///
/// Files that are already present and pass the sanity checks are kept, so a second call does not
/// touch the network. Each file is written to a temporary name in the same directory, checked
/// (announced size, JSON well-formedness, safetensors header) and then renamed into place, so an
/// interrupted download never leaves a truncated file under its final name.
///
/// Only ever call this on explicit user request: it transfers tens to hundreds of megabytes.
pub async fn ensure_model(
    models_dir: &Path,
    repo_id: &str,
    progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<PathBuf> {
    ensure_model_from(HUGGING_FACE, models_dir, repo_id, progress).await
}

/// [`ensure_model`] against an arbitrary host (tests use an in-process server).
pub(crate) async fn ensure_model_from(
    base_url: &str,
    models_dir: &Path,
    repo_id: &str,
    progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<PathBuf> {
    let dir = model_dir(models_dir, repo_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let client = net::http_client(None)?;
    for file in MODEL_FILES {
        let target = dir.join(file);
        if validate(file, target.clone()).await.is_ok() {
            continue;
        }
        let url = format!("{base_url}/{repo_id}/resolve/main/{file}");
        let partial = dir.join(format!(".{file}.part"));
        let outcome = fetch(&client, &url, file, &partial, progress).await;
        let outcome = match outcome {
            Ok(()) => validate(file, partial.clone()).await,
            Err(err) => Err(err),
        };
        if let Err(err) = outcome {
            // Best effort: the partial file is useless, and a leftover is overwritten anyway.
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(err);
        }
        tokio::fs::rename(&partial, &target).await?;
    }
    Ok(dir)
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    file: &'static str,
    partial: &Path,
    progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<()> {
    let limit = if file.ends_with(".json") {
        MAX_JSON_BYTES
    } else {
        MAX_WEIGHTS_BYTES
    };
    let mut response = tokio::time::timeout(IDLE_TIMEOUT, client.get(url).send())
        .await
        .map_err(|_| Error::other(format!("timed out connecting to {url}")))??;
    let status = response.status();
    if !status.is_success() {
        let hint = if status == reqwest::StatusCode::NOT_FOUND {
            " (check the model id: the repository must provide config.json, tokenizer.json and \
             model.safetensors)"
        } else {
            ""
        };
        return Err(Error::other(format!(
            "downloading {url} failed with HTTP {status}{hint}"
        )));
    }
    let total = response.content_length();
    if total.is_some_and(|total| total > limit) {
        return Err(Error::other(format!(
            "{url} announces {} bytes, more than the {limit} byte limit for {file}",
            total.unwrap_or_default()
        )));
    }

    let mut output = tokio::fs::File::create(partial).await?;
    let mut downloaded = 0_u64;
    loop {
        let chunk = tokio::time::timeout(IDLE_TIMEOUT, response.chunk())
            .await
            .map_err(|_| Error::other(format!("download of {url} stalled")))??;
        let Some(chunk) = chunk else { break };
        downloaded += chunk.len() as u64;
        if downloaded > limit {
            return Err(Error::other(format!(
                "{url} exceeds the {limit} byte limit for {file}"
            )));
        }
        output.write_all(&chunk).await?;
        progress(DownloadProgress {
            file,
            downloaded,
            total,
        });
    }
    output.flush().await?;
    output.sync_all().await?;
    if downloaded == 0 || total.is_some_and(|total| total != downloaded) {
        return Err(Error::other(format!(
            "download of {url} is incomplete: received {downloaded} bytes, expected {}",
            total.map_or_else(|| "a non-empty body".to_owned(), |total| total.to_string())
        )));
    }
    progress(DownloadProgress {
        file,
        downloaded,
        total: Some(downloaded),
    });
    Ok(())
}

/// Structural sanity check of one model file, on the blocking pool.
async fn validate(file: &'static str, path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || validate_blocking(file, &path)).await?
}

fn validate_blocking(file: &str, path: &Path) -> Result<()> {
    use std::io::Read;

    let size = std::fs::metadata(path)?.len();
    let bad = |reason: String| Error::integrity(format!("{}: {reason}", path.display()));
    if file.ends_with(".json") {
        if size == 0 || size > MAX_JSON_BYTES {
            return Err(bad(format!("implausible size of {size} bytes")));
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|err| bad(format!("not valid JSON ({err})")))?;
        if !value.is_object() {
            return Err(bad("not a JSON object".to_owned()));
        }
        return Ok(());
    }
    // safetensors: u64 little-endian header length, a JSON header, then the tensor data.
    let mut prefix = [0_u8; 9];
    let mut reader = std::fs::File::open(path)?;
    reader
        .read_exact(&mut prefix)
        .map_err(|_| bad("too short to be a safetensors file".to_owned()))?;
    let mut length = [0_u8; 8];
    length.copy_from_slice(&prefix[..8]);
    let header = u64::from_le_bytes(length);
    if header < 2 || header.saturating_add(8) > size || prefix[8] != b'{' {
        return Err(bad("not a safetensors file (bad header)".to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::Path as UrlPath;
    use axum::http::StatusCode;
    use axum::routing::get;

    use super::*;

    fn safetensors_bytes() -> Vec<u8> {
        let header = br#"{"__metadata__":{}}"#;
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header);
        bytes
    }

    /// Fake hub serving `/{owner}/{name}/resolve/main/{file}`; `broken` serves a corrupt weights
    /// file.
    async fn hub(broken: bool) -> (String, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let app = Router::new().route(
            "/{owner}/{name}/resolve/main/{file}",
            get(
                move |UrlPath((owner, _, file)): UrlPath<(String, String, String)>| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if owner != "acme" {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        }
                        let body = match file.as_str() {
                            "model.safetensors" if broken => b"garbage that is no model".to_vec(),
                            "model.safetensors" => safetensors_bytes(),
                            _ => br#"{"ok":true}"#.to_vec(),
                        };
                        (StatusCode::OK, body)
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, requests)
    }

    #[test]
    fn model_dir_rejects_ids_that_escape() {
        let root = Path::new("/models");
        assert_eq!(
            model_dir(root, "sentence-transformers/all-MiniLM-L6-v2").unwrap(),
            root.join("sentence-transformers--all-MiniLM-L6-v2")
        );
        for bad in ["", "../etc", "a/b/c", "a//b", "a/..", "a b", "/abs"] {
            assert!(model_dir(root, bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn downloads_atomically_and_only_once() {
        let (url, requests) = hub(false).await;
        let dir = tempfile::tempdir().unwrap();
        let events = std::sync::Mutex::new(Vec::new());
        let progress = |p: DownloadProgress| events.lock().unwrap().push(p);

        let model = ensure_model_from(&url, dir.path(), "acme/tiny", &progress)
            .await
            .unwrap();
        assert_eq!(model, dir.path().join("acme--tiny"));
        assert!(model_files_present(&model));
        assert_eq!(
            std::fs::read(model.join("model.safetensors")).unwrap(),
            safetensors_bytes()
        );
        assert_eq!(
            std::fs::read_dir(&model).unwrap().count(),
            3,
            "no temporary files left"
        );
        let last = *events.lock().unwrap().last().unwrap();
        assert_eq!(last.file, "model.safetensors");
        assert_eq!(last.total, Some(last.downloaded));

        // Present and sane: no further request.
        ensure_model_from(&url, dir.path(), "acme/tiny", &progress)
            .await
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn corrupt_or_missing_files_fail_without_leaving_anything_behind() {
        let (url, _) = hub(true).await;
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_model_from(&url, dir.path(), "acme/tiny", &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Integrity(_)), "{err}");
        let model = dir.path().join("acme--tiny");
        assert!(!model_files_present(&model));
        assert!(!model.join("model.safetensors").exists());
        assert!(!model.join(".model.safetensors.part").exists());

        let err = ensure_model_from(&url, dir.path(), "nobody/tiny", &|_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }
}
