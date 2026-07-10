# Enclave - llama-server sidecar fetcher
# Downloads the correct llama.cpp release binary for the current platform
# and places it at the expected Tauri externalBin path.
#
# Usage:
#   powershell -File .\scripts\fetch-sidecar.ps1
#
# ADR-0003: The app is self-contained; this script is the one-time setup step.

param(
    [string]$Version = "latest"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoOwner = "ggerganov"
$RepoName  = "llama.cpp"
$OutDir    = Join-Path $PSScriptRoot "..\src-tauri\binaries"

if ($Version -eq "latest") {
    Write-Host "Fetching latest llama.cpp release tag..."
    $rel = Invoke-RestMethod "https://api.github.com/repos/$RepoOwner/$RepoName/releases/latest" -UseBasicParsing
    $Version = $rel.tag_name
    Write-Host "  Latest version: $Version"
}

$arch    = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
$is64    = $arch -eq "X64"
$isArm64 = $arch -eq "Arm64"

if (-not ($is64 -or $isArm64)) {
    Write-Error "Unsupported architecture: $arch"
}

$assetPattern = if ($isArm64) {
    "llama-*-bin-win-arm64.zip"
} else {
    "llama-*-bin-win-cuda-*-x64.zip"
}

$assetPatternCpu = "llama-*-bin-win-noavx-x64.zip"

$rel     = Invoke-RestMethod "https://api.github.com/repos/$RepoOwner/$RepoName/releases/tags/$Version" -UseBasicParsing
$assets  = $rel.assets

$asset = $assets | Where-Object { $_.name -like $assetPattern } | Select-Object -First 1
if (-not $asset) {
    Write-Warning "CUDA asset not found, falling back to CPU binary."
    $asset = $assets | Where-Object { $_.name -like $assetPatternCpu } | Select-Object -First 1
}
if (-not $asset) {
    Write-Error "No suitable llama.cpp Windows binary found for $Version."
}

Write-Host "Downloading: $($asset.name)"
$tmpZip = Join-Path $env:TEMP "llama-sidecar.zip"
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpZip -UseBasicParsing

$tmpDir = Join-Path $env:TEMP "llama-sidecar-extract"
if (Test-Path $tmpDir) { Remove-Item $tmpDir -Recurse -Force }
Expand-Archive -Path $tmpZip -DestinationPath $tmpDir

$serverBin = Get-ChildItem $tmpDir -Recurse -Filter "llama-server.exe" | Select-Object -First 1
if (-not $serverBin) {
    Write-Error "llama-server.exe not found in archive."
}

$triple = "x86_64-pc-windows-msvc"
if ($isArm64) { $triple = "aarch64-pc-windows-msvc" }

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }
$dest = Join-Path $OutDir "llama-server-$triple.exe"

Copy-Item $serverBin.FullName -Destination $dest -Force
Write-Host "Placed sidecar at: $dest"

Remove-Item $tmpZip -Force
Remove-Item $tmpDir -Recurse -Force

# CUDA builds need the matching CUDA runtime DLLs alongside the exe, or it
# launches and silently does nothing (no error, no listening port). Not
# needed for the ARM64/CPU (noavx) fallback builds.
if (-not $isArm64 -and $asset.name -like "*-cuda-*") {
    $cudartPattern = "cudart-llama-bin-win-cuda-*-x64.zip"
    $cudartAsset = $assets | Where-Object { $_.name -like $cudartPattern } | Select-Object -First 1

    if ($cudartAsset) {
        Write-Host "Downloading CUDA runtime: $($cudartAsset.name)"
        $cudaZip = Join-Path $env:TEMP "llama-cudart.zip"
        Invoke-WebRequest -Uri $cudartAsset.browser_download_url -OutFile $cudaZip -UseBasicParsing

        $cudaDir = Join-Path $env:TEMP "llama-cudart-extract"
        if (Test-Path $cudaDir) { Remove-Item $cudaDir -Recurse -Force }
        Expand-Archive -Path $cudaZip -DestinationPath $cudaDir

        $dlls = Get-ChildItem $cudaDir -Recurse -Filter "*.dll"
        foreach ($dll in $dlls) {
            Copy-Item $dll.FullName -Destination $OutDir -Force
        }
        Write-Host "Placed $($dlls.Count) CUDA runtime DLL(s) in: $OutDir"

        Remove-Item $cudaZip -Force
        Remove-Item $cudaDir -Recurse -Force
    } else {
        Write-Warning "No CUDA runtime asset found for $Version. The CUDA build of llama-server.exe will launch and silently exit without these DLLs — download 'cudart-llama-bin-win-cuda-*-x64.zip' from the same release manually and extract its DLLs into $OutDir."
    }
}

Write-Host "Done. You can now run the build/check."
