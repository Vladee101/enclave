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
    // The loader alone is not enough: a clean Windows 11 VM has
    // vulkan-1.dll and no device behind it (docs/clean-machine-check.md).
    // A Vulkan driver registers itself — in its display adapter's key on
    // current Intel / AMD / NVIDIA drivers, in Khronos\Vulkan\Drivers on old
    // ones.
    if system32.join("vulkan-1.dll").is_file() && vulkan_driver_registered(&system32) {
        return Flavor::Vulkan;
    }
    Flavor::Cpu
}

/// Display adapters' device class: each adapter's key names its Vulkan
/// driver (`VulkanDriverName`) when it has one.
const DISPLAY_CLASS: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
const KHRONOS_DRIVERS: &str = r"HKLM\SOFTWARE\Khronos\Vulkan\Drivers";

/// Ask reg.exe (part of every Windows) rather than bind the registry API
/// for one read; its value lines are not translated, only its messages.
fn vulkan_driver_registered(system32: &Path) -> bool {
    let query = |args: &[&str]| {
        let mut cmd = std::process::Command::new(system32.join("reg.exe"));
        cmd.args(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    };
    lists_vulkan_driver(&query(&["query", DISPLAY_CLASS, "/s", "/v", "VulkanDriverName"]))
        || lists_vulkan_driver(&query(&["query", KHRONOS_DRIVERS]))
}

/// A value line of `reg query` output — "    <name>    REG_<type>    <data>" —
/// that names a driver manifest: `VulkanDriverName` under an adapter, or
/// the manifest path itself as the value name under Khronos.
fn lists_vulkan_driver(reg_output: &str) -> bool {
    reg_output.lines().any(|line| {
        let line = line.trim();
        line.contains("    REG_") && (line.starts_with("VulkanDriverName") || line.to_ascii_lowercase().contains(".json"))
    })
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
    if !all.iter().all(|s| s.state == "ready") {
        return None;
    }
    // Every start, not only after unpacking: engines downloaded by a build
    // that did not do this get the runtime too.
    match crate::db::embedded::tools_dir(app) {
        Ok(from) => {
            if let Err(e) = copy_vc_runtime(&from, &dir) {
                tracing::warn!("Visual C++ runtime not copied next to the engine: {e:#}");
            }
        }
        Err(e) => tracing::warn!("Visual C++ runtime not copied next to the engine: {e:#}"),
    }
    Some(dir)
}

/// The Visual C++ runtime llama.cpp's Windows builds link against. Their
/// release archives do not carry it, so on a machine without the
/// redistributable llama-server.exe does not start — "VCRUNTIME140.dll was
/// not found", seen on a clean Windows 11 VM (docs/clean-machine-check.md);
/// a development machine always has it installed. The installer already
/// carries these files next to PostgreSQL (scripts/fetch-postgres.ps1), and
/// they are copied next to the engine: app-local, as Microsoft allows for
/// the redistributable DLLs. PSAPI.DLL, also imported, is part of Windows.
const VC_RUNTIME: [&str; 3] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];

/// Copy the runtime files `dir` lacks from `from`. A file already there is
/// left alone.
fn copy_vc_runtime(from: &Path, dir: &Path) -> Result<()> {
    for name in VC_RUNTIME {
        let to = dir.join(name);
        if to.is_file() {
            continue;
        }
        std::fs::copy(from.join(name), &to)
            .with_context(|| format!("could not copy {name} from {} to {}", from.display(), dir.display()))?;
    }
    Ok(())
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
                name: if a.key == "engine" { f.label() } else { "CUDA" }.to_string(),
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
    fn a_vulkan_driver_is_one_registered_not_a_loader_on_disk() {
        // This machine: Intel and NVIDIA register theirs per adapter.
        let adapters = concat!(
            "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0001\r\n",
            "    VulkanDriverName    REG_MULTI_SZ    C:\\WINDOWS\\System32\\DriverStore\\FileRepository\\nvaci.inf_amd64_0be28d2a022d2f00\\nv-vk64.json\r\n",
            "\r\nEnd of search: 1 match(es) found.\r\n",
        );
        assert!(lists_vulkan_driver(adapters));
        // An old driver: the manifest path is the value name.
        let khronos = concat!(
            "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Khronos\\Vulkan\\Drivers\r\n",
            "    C:\\Windows\\System32\\amd-vulkan64.json    REG_DWORD    0x0\r\n",
        );
        assert!(lists_vulkan_driver(khronos));
        // The VM: nothing found, in any language; reg.exe missing altogether.
        assert!(!lists_vulkan_driver("\r\nEnd of search: 0 match(es) found.\r\n"));
        assert!(!lists_vulkan_driver("\r\nПоиск завершен: найдено совпадений: 0.\r\n"));
        assert!(!lists_vulkan_driver(""));
    }

    #[test]
    fn the_vc_runtime_is_copied_next_to_the_engine_once() {
        let root = std::env::temp_dir().join(format!("enclave-vcrt-{}", std::process::id()));
        let (from, dir) = (root.join("pg-bin"), root.join("engine"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        for name in VC_RUNTIME {
            std::fs::write(from.join(name), name).unwrap();
        }
        // One already there (an archive that does carry it) stays as it is.
        std::fs::write(dir.join("msvcp140.dll"), "the engine's own").unwrap();

        copy_vc_runtime(&from, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("vcruntime140.dll")).unwrap(), "vcruntime140.dll");
        assert_eq!(std::fs::read_to_string(dir.join("vcruntime140_1.dll")).unwrap(), "vcruntime140_1.dll");
        assert_eq!(std::fs::read_to_string(dir.join("msvcp140.dll")).unwrap(), "the engine's own");

        // Missing at the source: an error, not a silent half-copy.
        std::fs::remove_file(dir.join("vcruntime140.dll")).unwrap();
        std::fs::remove_file(from.join("vcruntime140.dll")).unwrap();
        assert!(copy_vc_runtime(&from, &dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

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
