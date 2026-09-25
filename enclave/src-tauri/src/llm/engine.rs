//! The inference engine — llama.cpp's llama-server — fetched on first run
//! for the machine it runs on (ADR-0025).
//!
//! One pinned llama.cpp release, three builds, chosen by the hardware:
//!
//! - **CUDA 12.4** (254 + 391 MB with its runtime): an NVIDIA GPU whose
//!   driver supports CUDA 12.4 or newer (`nvidia-smi` says which);
//! - **Vulkan** (32 MB): any GPU with a Vulkan driver — Intel, AMD, NVIDIA;
//! - **CPU** (19 MB): the fallback, slow but everywhere.
//!
//! `ENCLAVE_ENGINE=cuda|vulkan|cpu` overrides the choice. Archives go
//! through the same verified, resumable download as the models, then are
//! unpacked flat into `{app_data}/engine/<build>/` — llama-server.exe finds
//! its backends (`ggml-cuda.dll`, `ggml-vulkan.dll`) next to itself.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::{AppHandle, Manager};
use tracing::info;

use super::models::{fetch_verified, ModelStatus};

/// The llama.cpp release every build comes from — the one the app was
/// tested with.
const RELEASE: &str = "b11124";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Cuda,
    Vulkan,
    Cpu,
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::Cuda => "cuda-12.4",
            Flavor::Vulkan => "vulkan",
            Flavor::Cpu => "cpu",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Flavor::Cuda => "NVIDIA CUDA",
            Flavor::Vulkan => "Vulkan",
            Flavor::Cpu => "CPU",
        }
    }
}

/// One release archive. Size and SHA-256 are GitHub's own (the release
/// API's asset digest), pinned here.
struct Archive {
    key:    &'static str,
    file:   &'static str,
    sha256: &'static str,
    size:   u64,
}

const CUDA: [Archive; 2] = [
    Archive {
        key:    "engine",
        file:   "llama-b11124-bin-win-cuda-12.4-x64.zip",
        sha256: "9fb910164dcb2f26c89343142581e66e678a00f9b4999888a3f0e5aac00d2b63",
        size:   253_784_032,
    },
    Archive {
        key:    "engine-cuda-runtime",
        file:   "cudart-llama-bin-win-cuda-12.4-x64.zip",
        sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6",
        size:   391_443_627,
    },
];

const VULKAN: [Archive; 1] = [Archive {
    key:    "engine",
    file:   "llama-b11124-bin-win-vulkan-x64.zip",
    sha256: "ece009eff2a18884246b4aef785ce4efd9eb36f4fc38e3d6727af6fe53bd2c75",
    size:   31_972_308,
}];

const CPU: [Archive; 1] = [Archive {
    key:    "engine",
    file:   "llama-b11124-bin-win-cpu-x64.zip",
    sha256: "7eb4e7475f1730e0845e079e41f2e79b0c6de71d86731f755197129620a5bd28",
    size:   18_558_390,
}];

fn archives(flavor: Flavor) -> &'static [Archive] {
    match flavor {
        Flavor::Cuda => &CUDA,
        Flavor::Vulkan => &VULKAN,
        Flavor::Cpu => &CPU,
    }
}

fn url(archive: &Archive) -> String {
    format!("https://github.com/ggml-org/llama.cpp/releases/download/{RELEASE}/{}", archive.file)
}

/// "CUDA Version: 12.8" from nvidia-smi's banner → (12, 8). Newer drivers
/// label it "CUDA UMD Version: 13.3" (seen with driver 610.88) — the same
/// number, which the split below finds either way.
fn cuda_version(nvidia_smi_output: &str) -> Option<(u32, u32)> {
    let rest = nvidia_smi_output.split("CUDA Version:").nth(1)
        .or_else(|| nvidia_smi_output.split("CUDA UMD Version:").nth(1))?
        .trim_start();
    let version: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn detect() -> Flavor {
    if let Ok(forced) = std::env::var("ENCLAVE_ENGINE") {
        match forced.to_ascii_lowercase().as_str() {
            "cuda" => return Flavor::Cuda,
            "vulkan" => return Flavor::Vulkan,
            "cpu" => return Flavor::Cpu,
            other => tracing::warn!("ENCLAVE_ENGINE={other} is not cuda, vulkan or cpu — detecting instead"),
        }
    }
    let system32 = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32");
    // nvidia-smi ships with the NVIDIA driver; its banner states the newest
    // CUDA the driver supports. The 12.4 build needs at least that.
    let mut cmd = std::process::Command::new(system32.join("nvidia-smi.exe"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    if let Ok(out) = cmd.output() {
        if out.status.success() && cuda_version(&String::from_utf8_lossy(&out.stdout)).is_some_and(|v| v >= (12, 4)) {
            return Flavor::Cuda;
        }
    }
    if system32.join("vulkan-1.dll").is_file() {
        return Flavor::Vulkan;
    }
    Flavor::Cpu
}

/// The build for this machine, decided once per run.
pub fn flavor() -> Flavor {
    static FLAVOR: OnceLock<Flavor> = OnceLock::new();
    *FLAVOR.get_or_init(|| {
        let f = detect();
        info!("Inference engine for this machine: llama.cpp {RELEASE} {}", f.name());
        f
    })
}

fn engine_dir(app: &AppHandle) -> Result<PathBuf> {
    Ok(app.path().app_data_dir().context("no app data directory")?.join("engine").join(flavor().name()))
}

/// The installed engine's directory, if it is complete — for
/// `llm::llama_server_exe`.
pub fn installed_dir(app: &AppHandle) -> Option<PathBuf> {
    let dir = engine_dir(app).ok()?;
    let all = status(app).ok()?;
    all.iter().all(|s| s.state == "ready").then_some(dir)
}

#[derive(Serialize, Deserialize)]
struct Unpacked {
    sha256: String,
}

fn manifest(dir: &Path) -> BTreeMap<String, Unpacked> {
    std::fs::read(dir.join("manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn status(app: &AppHandle) -> Result<Vec<ModelStatus>> {
    let f = flavor();
    let dir = engine_dir(app)?;
    let unpacked = manifest(&dir);
    let has_server = dir.join("llama-server.exe").is_file();
    Ok(archives(f)
        .iter()
        .map(|a| {
            let ready = has_server && unpacked.get(a.file).is_some_and(|u| u.sha256 == a.sha256);
            let partial = std::fs::metadata(dir.with_file_name(format!("{}.part", a.file))).map(|m| m.len()).unwrap_or(0);
            ModelStatus {
                key: a.key,
                label: if a.key == "engine" {
                    format!("llama.cpp engine ({})", f.label())
                } else {
                    "CUDA runtime for the engine".to_string()
                },
                state: if ready { "ready" } else { "missing" },
                size: a.size,
                partial,
            }
        })
        .collect())
}

/// Every file of the archive into `dir`, flattened: the release zips keep
/// the exe and its DLLs in one folder, and so must we.
fn unpack(zip_path: &Path, dir: &Path) -> Result<usize> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        // enclosed_name rejects absolute paths and "..": nothing lands
        // outside `dir`.
        let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(PathBuf::from)) else {
            continue;
        };
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        std::fs::write(dir.join(name), bytes)?;
        count += 1;
    }
    Ok(count)
}

pub async fn download_missing(app: &AppHandle, http: &reqwest::Client) -> Result<()> {
    let f = flavor();
    let dir = engine_dir(app)?;
    std::fs::create_dir_all(&dir)?;
    let states = status(app)?;
    for (a, s) in archives(f).iter().zip(states) {
        if s.state == "ready" {
            continue;
        }
        // Next to the engine directory, not in it: a half-downloaded
        // archive is not part of the engine.
        let part = dir.with_file_name(format!("{}.part", a.file));
        fetch_verified(app, http, a.key, &url(a), a.sha256, a.size, &part)
            .await
            .with_context(|| format!("llama.cpp {}", a.file))?;
        let (zip, target) = (part.clone(), dir.clone());
        let files = tauri::async_runtime::spawn_blocking(move || unpack(&zip, &target)).await??;
        std::fs::remove_file(&part)?;
        let mut m = manifest(&dir);
        m.insert(a.file.to_string(), Unpacked { sha256: a.sha256.to_string() });
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&m)?)?;
        info!("llama.cpp {}: {files} files unpacked into {}", a.file, dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_version_is_read_from_the_nvidia_smi_banner() {
        let banner = "| NVIDIA-SMI 581.57   Driver Version: 581.57   CUDA Version: 13.0     |";
        assert_eq!(cuda_version(banner), Some((13, 0)));
        assert_eq!(cuda_version("| CUDA Version: 12.4 |"), Some((12, 4)));
        let newer = "| NVIDIA-SMI 610.88                 KMD Version: 610.88        CUDA UMD Version: 13.3     |";
        assert_eq!(cuda_version(newer), Some((13, 3)));
        assert!(cuda_version("| CUDA Version: 12.2 |").is_some_and(|v| v < (12, 4)));
        assert_eq!(cuda_version("no gpu here"), None);
    }

    #[test]
    fn archives_are_unpacked_flat_and_never_outside_the_directory() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("enclave-engine-{}", std::process::id()));
        let dir = root.join("engine");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = root.join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("build/bin/llama-server.exe", opts).unwrap();
            z.write_all(b"exe").unwrap();
            z.start_file("ggml-cuda.dll", opts).unwrap();
            z.write_all(b"dll").unwrap();
            z.start_file("../escape.txt", opts).unwrap();
            z.write_all(b"no").unwrap();
            z.finish().unwrap();
        }
        assert_eq!(unpack(&zip_path, &dir).unwrap(), 2);
        assert_eq!(std::fs::read(dir.join("llama-server.exe")).unwrap(), b"exe");
        assert!(dir.join("ggml-cuda.dll").is_file());
        assert!(!root.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
