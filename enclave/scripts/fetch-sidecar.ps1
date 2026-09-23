# Enclave - llama-server fetcher
# Downloads a llama.cpp Windows release and unpacks ALL of it (exe + DLLs)
# into src-tauri/binaries/llama/, where the app runs llama-server in place.
#
# Usage:
#   powershell -File .\scripts\fetch-sidecar.ps1                  # newest build, CUDA 12.4
#   powershell -File .\scripts\fetch-sidecar.ps1 -Build b11124    # pin a build
#   powershell -File .\scripts\fetch-sidecar.ps1 -Cuda 13.4       # newer CUDA (driver must support it)
#
# ADR-0003: the app is self-contained; this script is the one-time setup step.
#
# Why the whole archive: current builds ship a ~9 KB llama-server.exe launcher;
# the server is llama-server-impl.dll, and the ggml backends (ggml-cuda.dll,
# ggml-cpu-*.dll) are discovered next to the exe. Copying only the exe - what
# this script used to do - gives a server that starts and does nothing.
#
# Why not "latest": the release GitHub marks latest (e.g. v0.4.1) carries no
# binaries; builds are published as bNNNNN pre-releases.
#
# Why CUDA 12.4 by default: it runs on any driver that supports CUDA 12.4+,
# while the 13.x build needs a driver at least that new (check the "CUDA
# Version" / "CUDA UMD Version" line of nvidia-smi).

param(
    [string]$Build = "",
    [string]$Cuda  = "12.4"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference    = "SilentlyContinue"   # Invoke-WebRequest is ~10x slower with the progress bar

$Repo   = "ggml-org/llama.cpp"
$OutDir = Join-Path $PSScriptRoot "..\src-tauri\binaries\llama"

$arch = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
if ($arch -ne "X64") {
    Write-Error "Only x64 is scripted so far (got $arch)."
}

$mainPattern   = "llama-*-bin-win-cuda-$Cuda-x64.zip"
$cudartPattern = "cudart-llama-bin-win-cuda-$Cuda-x64.zip"

if ($Build) {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/tags/$Build" -UseBasicParsing
} else {
    Write-Host "Looking for the newest build with a Windows CUDA $Cuda asset..."
    $releases = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases?per_page=20" -UseBasicParsing
    $rel = $releases | Where-Object { $_.assets | Where-Object { $_.name -like $mainPattern } } | Select-Object -First 1
    if (-not $rel) { Write-Error "No recent release has an asset matching $mainPattern." }
}
Write-Host "Build: $($rel.tag_name)"

$main   = $rel.assets | Where-Object { $_.name -like $mainPattern }   | Select-Object -First 1
$cudart = $rel.assets | Where-Object { $_.name -like $cudartPattern } | Select-Object -First 1
if (-not $main)   { Write-Error "$($rel.tag_name) has no asset matching $mainPattern." }
if (-not $cudart) { Write-Error "$($rel.tag_name) has no asset matching $cudartPattern - without the CUDA runtime DLLs the server exits silently." }

if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
New-Item -ItemType Directory -Path $OutDir | Out-Null

foreach ($asset in @($main, $cudart)) {
    Write-Host "Downloading $($asset.name) ($([math]::Round($asset.size / 1MB)) MB)..."
    $zip = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing
    $tmp = Join-Path $env:TEMP "enclave-llama-extract"
    if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $tmp
    # Flatten: exe and DLLs must share one directory.
    Get-ChildItem $tmp -Recurse -File | Copy-Item -Destination $OutDir -Force
    Remove-Item $zip -Force
    Remove-Item $tmp -Recurse -Force
}

$exe = Join-Path $OutDir "llama-server.exe"
if (-not (Test-Path $exe)) { Write-Error "llama-server.exe not found after unpacking." }
Set-Content -Path (Join-Path $OutDir "BUILD.txt") -Value "$($rel.tag_name) cuda-$Cuda" -Encoding utf8

Write-Host "Unpacked $((Get-ChildItem $OutDir -File).Count) files into $OutDir"
& $exe --version
