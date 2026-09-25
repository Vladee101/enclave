# Enclave - Windows installer (NSIS)
# Builds the release app and its per-user installer (ADR-0014, ADR-0024,
# ADR-0025): the app and the embedded PostgreSQL go in; the models and the
# llama.cpp engine do not - the app downloads them on first run for the
# machine it lands on.
#
# Usage (from enclave/):
#   powershell -File .\scripts\build-installer.ps1
#
# Needs src-tauri\binaries\pg (scripts\fetch-postgres.ps1). The installer
# lands in target\release\bundle\nsis\.
#
# The bundle settings live in src-tauri\tauri.bundle.json, merged only here:
# in tauri.conf.json the resources would be required by every cargo build,
# CI's included.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$Pg   = Join-Path $Root "src-tauri\binaries\pg\bin\postgres.exe"
if (-not (Test-Path $Pg)) { Write-Error "No embedded PostgreSQL at $Pg - run scripts\fetch-postgres.ps1 first." }

# The Visual C++ runtime linked into enclave.exe itself, so a machine
# without the redistributable runs it (PostgreSQL carries its own copy,
# llama.cpp's archives theirs). Set here, not in .cargo/config.toml: it
# changes every crate's build, and development does not need it.
$env:RUSTFLAGS = "-C target-feature=+crt-static"

Push-Location $Root
try {
    pnpm tauri build --config src-tauri/tauri.bundle.json
    if ($LASTEXITCODE -ne 0) { Write-Error "tauri build failed (exit $LASTEXITCODE)." }
} finally {
    Pop-Location
}

$installer = Get-ChildItem (Join-Path $Root "target\release\bundle\nsis") -Filter "*.exe" |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
Write-Host ("Installer: {0} ({1:N1} MB)" -f $installer.FullName, ($installer.Length / 1MB))
