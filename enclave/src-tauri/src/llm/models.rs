//! Model weights, fetched on first run (ADR-0024); the inference engine
//! beside them is `engine.rs`.
//!
//! The installer carries no weights (2.8 GB); the app downloads them once
//! from the models' official repositories, or takes files the user already
//! has (an offline machine). Either way a file counts as installed only
//! when `manifest.json` records it — size and SHA-256 checked at download,
//! the user's own choice at import — so a stray or half-written file is
//! never mistaken for a model.
//!
//! Downloads resume: bytes go to `<name>.part`, which a later attempt
//! continues with an HTTP Range request; the hash covers the whole file
//! either way, and only a verified file is renamed into place.

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tracing::{info, warn};

/// One model file the app needs.
pub struct ModelFile {
    /// Stable id, used by the frontend.
    pub key:       &'static str,
    /// What the user sees.
    pub label:     &'static str,
    /// The name `llm::model_path` looks for in the models directory.
    pub file_name: &'static str,
    pub url:       &'static str,
    pub sha256:    &'static str,
    pub size:      u64,
}

/// Both models, from their official repositories; licenses Apache 2.0
/// (ADR-0024 — the previous chat model's license barred commercial use).
pub const MODELS: [ModelFile; 2] = [
    ModelFile {
        key:       "chat",
        label:     "Qwen3-4B (answers)",
        file_name: "base.gguf",
        url:       "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf",
        sha256:    "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5",
        size:      2_497_280_256,
    },
    ModelFile {
        key:       "embed",
        label:     "nomic-embed-text v1.5 (search)",
        file_name: "embed.gguf",
        url:       "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.f16.gguf",
        sha256:    "f7af6f66802f4df86eda10fe9bbcfc75c39562bed48ef6ace719a251cf1c2fdb",
        size:      274_290_560,
    },
];

/// What `manifest.json` knows about an installed file.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Installed {
    size:   u64,
    sha256: String,
    /// "download" (verified against the pinned hash) or "import" (the
    /// user's own file, accepted as is).
    source: String,
}

/// Per-item state for the frontend (a model file or an engine archive).
#[derive(Serialize, Debug)]
pub struct ModelStatus {
    pub key:        &'static str,
    pub label:      String,
    /// "ready", "missing", or "unverified" (a file is there, but nothing
    /// recorded it — e.g. copied in by hand before this existed).
    pub state:      &'static str,
    pub size:       u64,
    /// Bytes already in `<name>.part` — a download to resume.
    pub partial:    u64,
}

/// Progress of one file, sent as `models-progress`.
#[derive(Serialize, Clone)]
pub(crate) struct Progress {
    key:        &'static str,
    downloaded: u64,
    total:      u64,
}

/// Sent as `models-done` when a download run ends.
#[derive(Serialize, Clone)]
struct Done {
    ok:    bool,
    error: Option<String>,
}

/// `ENCLAVE_MODELS_DIR`, or `{app_data}/models` — where `llm::model_path`
/// looks too.
pub fn models_dir(app: &AppHandle) -> Result<PathBuf> {
    Ok(match std::env::var_os("ENCLAVE_MODELS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => app.path().app_data_dir().context("no app data directory")?.join("models"),
    })
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn read_manifest(dir: &Path) -> BTreeMap<String, Installed> {
    std::fs::read(manifest_path(dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record(dir: &Path, file_name: &str, installed: Installed) -> Result<()> {
    let mut manifest = read_manifest(dir);
    manifest.insert(file_name.to_string(), installed);
    let tmp = dir.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    std::fs::rename(&tmp, manifest_path(dir))?;
    Ok(())
}

/// Everything the first run needs: both models, then the inference
/// engine for this machine (`engine.rs`).
pub fn status(app: &AppHandle) -> Result<Vec<ModelStatus>> {
    let mut all = model_status(app)?;
    all.extend(super::engine::status(app)?);
    Ok(all)
}

fn model_status(app: &AppHandle) -> Result<Vec<ModelStatus>> {
    let dir = models_dir(app)?;
    let manifest = read_manifest(&dir);
    Ok(MODELS
        .iter()
        .map(|m| {
            let path = dir.join(m.file_name);
            let on_disk = std::fs::metadata(&path).map(|md| md.len()).ok();
            let state = match (on_disk, manifest.get(m.file_name)) {
                // A different pinned model (the constants changed) makes an
                // earlier download stale; an import stays the user's choice.
                (Some(len), Some(entry)) if entry.size == len && (entry.source == "import" || entry.sha256 == m.sha256) => "ready",
                (Some(_), _) => "unverified",
                (None, _) => "missing",
            };
            let partial = std::fs::metadata(dir.join(format!("{}.part", m.file_name))).map(|md| md.len()).unwrap_or(0);
            ModelStatus { key: m.key, label: m.label.to_string(), state, size: m.size, partial }
        })
        .collect())
}

/// Download whatever is not ready, one file after another, reporting
/// progress as events. Runs in the background; `models-done` says how it
/// ended.
pub async fn download_missing(app: AppHandle) {
    let result = async {
        let dir = models_dir(&app)?;
        std::fs::create_dir_all(&dir)?;
        let states = model_status(&app)?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        for (m, s) in MODELS.iter().zip(states) {
            if s.state != "ready" {
                download_one(&app, &http, &dir, m).await.with_context(|| m.label)?;
            }
        }
        super::engine::download_missing(&app, &http).await?;
        anyhow::Ok(())
    }
    .await;
    let done = match result {
        Ok(()) => Done { ok: true, error: None },
        Err(e) => {
            warn!("Model download failed: {e:#}");
            Done { ok: false, error: Some(format!("{e:#}")) }
        }
    };
    let _ = app.emit("models-done", done);
}

/// SHA-256 of what is already in a partial file, so a resumed download
/// still hashes the whole file.
pub(crate) fn hash_existing(path: &Path) -> Result<(Sha256, u64)> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
    }
    Ok((hasher, total))
}

/// Pauses before retrying a download that failed on the network.
const RETRY_DELAYS: [Duration; 3] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(10)];

/// Download `url` into `part`, resuming what is already there, and check
/// the whole file against `size` and `sha256`. On a hash mismatch the
/// partial file is removed (resuming it cannot fix it). Progress goes out
/// as `models-progress` under `key`. Shared by model files and the engine
/// archives (`engine.rs`).
///
/// Network failures are retried, each attempt resuming where the last one
/// stopped: through a local proxy the first connection timed out now and
/// then (both first attempts on the development machine), and a user
/// should not have to know that clicking again helps.
pub(crate) async fn fetch_verified(
    app: &AppHandle,
    http: &reqwest::Client,
    key: &'static str,
    url: &str,
    sha256: &str,
    size: u64,
    part: &Path,
) -> Result<()> {
    let mut delays = RETRY_DELAYS.iter();
    loop {
        match fetch_once(app, http, key, url, sha256, size, part).await {
            Err(e) if e.chain().any(|c| c.is::<reqwest::Error>()) => match delays.next() {
                Some(delay) => {
                    warn!("Download of {url} interrupted ({e:#}); retrying in {} s", delay.as_secs());
                    tokio::time::sleep(*delay).await;
                }
                None => return Err(e),
            },
            result => return result,
        }
    }
}

async fn fetch_once(
    app: &AppHandle,
    http: &reqwest::Client,
    key: &'static str,
    url: &str,
    sha256: &str,
    size: u64,
    part: &Path,
) -> Result<()> {
    let (hasher, mut have) = {
        let part = part.to_path_buf();
        tauri::async_runtime::spawn_blocking(move || hash_existing(&part)).await??
    };
    let mut hasher = hasher;
    if have > size {
        // Not a prefix of this file (something else was pinned before).
        std::fs::remove_file(part)?;
        hasher = Sha256::new();
        have = 0;
    }
    info!("Downloading {url} ({have} of {size} bytes already here)");

    if have < size {
        let mut request = http.get(url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let response = request.send().await?.error_for_status()?;
        if have > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            // The server ignored the range: start over rather than append
            // the whole file to its own beginning.
            have = 0;
            hasher = Sha256::new();
            std::fs::remove_file(part)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
        let mut stream = response.bytes_stream();
        let mut last_event = Instant::now();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("connection lost — start the download again to resume")?;
            file.write_all(&chunk)?;
            hasher.update(&chunk);
            have += chunk.len() as u64;
            if have > size {
                bail!("the server sent more than the expected {size} bytes");
            }
            if last_event.elapsed() > Duration::from_millis(250) {
                let _ = app.emit("models-progress", Progress { key, downloaded: have, total: size });
                last_event = Instant::now();
            }
        }
        file.flush()?;
    }
    let _ = app.emit("models-progress", Progress { key, downloaded: have, total: size });

    anyhow::ensure!(have == size, "download ended at {have} of {size} bytes — start it again to resume");
    let digest = format!("{:x}", hasher.finalize());
    if digest != sha256 {
        let _ = std::fs::remove_file(part);
        bail!("checksum mismatch (got {digest}, expected {sha256}); the partial file was removed");
    }
    Ok(())
}

async fn download_one(app: &AppHandle, http: &reqwest::Client, dir: &Path, m: &ModelFile) -> Result<()> {
    let part = dir.join(format!("{}.part", m.file_name));
    fetch_verified(app, http, m.key, m.url, m.sha256, m.size, &part).await?;
    let target = dir.join(m.file_name);
    // A file from before (unverified, or another model) is replaced; a hard
    // link to someone else's copy (Ollama's) is only unlinked, not changed.
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&part, &target)?;
    record(dir, m.file_name, Installed { size: m.size, sha256: m.sha256.to_string(), source: "download".into() })?;
    info!("{} installed as {}", m.label, target.display());
    Ok(())
}

/// Use a file the user already has instead of downloading (an offline
/// machine, a model copied from elsewhere). Accepted if it is a GGUF file;
/// linked when on the same volume, copied otherwise.
pub fn import(app: &AppHandle, key: &str, source: &Path) -> Result<()> {
    let m = MODELS.iter().find(|m| m.key == key).with_context(|| format!("unknown model {key}"))?;
    let mut magic = [0u8; 4];
    std::fs::File::open(source)
        .with_context(|| format!("cannot open {}", source.display()))?
        .read_exact(&mut magic)
        .context("the file is too short to be a model")?;
    anyhow::ensure!(&magic == b"GGUF", "{} is not a GGUF model file", source.display());

    let dir = models_dir(app)?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(m.file_name);
    let _ = std::fs::remove_file(&target);
    if std::fs::hard_link(source, &target).is_err() {
        std::fs::copy(source, &target).context("could not copy the model file")?;
    }
    let (hasher, size) = hash_existing(&target)?;
    let digest = format!("{:x}", hasher.finalize());
    if digest != m.sha256 {
        warn!("{} imported from {}: not the pinned file (sha256 {digest}) — accepted as the user's choice", m.label, source.display());
    }
    record(&dir, m.file_name, Installed { size, sha256: digest, source: "import".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resumed_hash_equals_the_whole_files_hash() {
        let dir = std::env::temp_dir().join(format!("enclave-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("x.part");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&part, &data[..1_234_567]).unwrap();
        let (mut hasher, have) = hash_existing(&part).unwrap();
        assert_eq!(have, 1_234_567);
        hasher.update(&data[1_234_567..]);
        assert_eq!(format!("{:x}", hasher.finalize()), format!("{:x}", Sha256::digest(&data)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_manifest_records_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("enclave-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        record(&dir, "base.gguf", Installed { size: 7, sha256: "ab".into(), source: "import".into() }).unwrap();
        record(&dir, "embed.gguf", Installed { size: 9, sha256: "cd".into(), source: "download".into() }).unwrap();
        let m = read_manifest(&dir);
        assert_eq!(m.len(), 2);
        assert_eq!(m["base.gguf"].size, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
