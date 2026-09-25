# Enclave - embedded PostgreSQL builder (ADR-0014)
# Lays out a trimmed PostgreSQL 18 + pgvector in src-tauri/binaries/pg/,
# which the app starts itself when no *_DATABASE_URL is set.
#
# Usage:
#   powershell -File .\scripts\fetch-postgres.ps1
#       # PostgreSQL from C:\Program Files\PostgreSQL\18, pgvector downloaded
#   powershell -File .\scripts\fetch-postgres.ps1 -PgSource D:\pgsql
#       # an unpacked EDB "binaries" zip (its pgsql folder) instead
#   powershell -File .\scripts\fetch-postgres.ps1 -PgvectorZip .\pgvector-0.8.3.zip
#       # pgvector source already downloaded (checked against the same hash)
#
# Needs Visual Studio Build Tools (C++ workload): pgvector has no official
# Windows binaries, so it is built here from source with MSVC against the
# same server headers - never taken as a third-party DLL (ADR-0014).
#
# What goes in (61 MB): the files the server, initdb, pg_ctl, pg_dump and
# pg_restore load (found with dumpbin /dependents), the extensions the
# migrations create (vector, pg_trgm - 001 creates it though nothing uses it)
# plus plpgsql and dict_snowball, and share/ without message translations.
# The source install is only read.

param(
    [string]$PgSource    = "C:\Program Files\PostgreSQL\18",
    [string]$PgvectorZip = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference    = "SilentlyContinue"

$PgMajor         = "18"
$PgvectorVersion = "0.8.3"
$PgvectorSha256  = "92c36a27a2078e4ec03aed4d3896777c2a14b12d9b3392d46f1c5988299fab4a"
$PgvectorUrl     = "https://github.com/pgvector/pgvector/archive/refs/tags/v$PgvectorVersion.zip"

$OutDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\src-tauri\binaries\pg"))
$Work   = Join-Path ([IO.Path]::GetTempPath()) "enclave-pg-build"

# Runtime files, by directory. Everything else in an EDB distribution
# (pgAdmin, StackBuilder and its wxWidgets, client tools, translations)
# is left out.
$BinFiles = @(
    "postgres.exe", "initdb.exe", "pg_ctl.exe", "pg_dump.exe", "pg_restore.exe",
    "icudt77.dll", "icuin77.dll", "icuuc77.dll",
    "libcrypto-3-x64.dll", "libssl-3-x64.dll", "libiconv-2.dll", "libintl-9.dll",
    "liblz4.dll", "libpq.dll", "libwinpthread-1.dll", "libxml2.dll", "libzstd.dll", "zlib1.dll"
)
$LibFiles       = @("plpgsql.dll", "dict_snowball.dll", "pg_trgm.dll")
$ShareDirs      = @("timezone", "timezonesets", "tsearch_data")
$ExtensionGlobs = @("plpgsql*", "pg_trgm*")

# --- source checks --------------------------------------------------------
$pgConfig = Join-Path $PgSource "bin\pg_config.exe"
if (-not (Test-Path $pgConfig)) { Write-Error "No PostgreSQL at $PgSource (bin\pg_config.exe missing)." }
$version = (& $pgConfig --version).Trim()
if ($version -notmatch "^PostgreSQL $PgMajor\.") { Write-Error "Need PostgreSQL $PgMajor, $PgSource has '$version'." }
foreach ($f in $BinFiles) {
    if (-not (Test-Path (Join-Path $PgSource "bin\$f"))) { Write-Error "$PgSource\bin\$f missing - a different build? Check the list with dumpbin /dependents." }
}
if (-not (Test-Path (Join-Path $PgSource "include\server\postgres.h"))) { Write-Error "$PgSource has no server headers (include\server) - needed to build pgvector." }
Write-Host "PostgreSQL: $version from $PgSource"

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { Write-Error "Visual Studio Build Tools not found (vswhere.exe missing)." }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { Write-Error "Visual Studio found, but without the C++ build tools (VC.Tools.x86.x64)." }
$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"

# --- pgvector source --------------------------------------------------------
if (Test-Path $Work) { Remove-Item $Work -Recurse -Force }
New-Item -ItemType Directory -Path $Work | Out-Null
$zip = Join-Path $Work "pgvector.zip"
if ($PgvectorZip) {
    Copy-Item $PgvectorZip $zip
} else {
    Write-Host "Downloading pgvector $PgvectorVersion source..."
    Invoke-WebRequest $PgvectorUrl -OutFile $zip -UseBasicParsing
}
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
if ($hash -ne $PgvectorSha256) { Write-Error "pgvector source hash mismatch: got $hash, expected $PgvectorSha256." }
Expand-Archive $zip -DestinationPath $Work
$pgvectorSrc = Join-Path $Work "pgvector-$PgvectorVersion"

# --- lay out the runtime ----------------------------------------------------
if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
foreach ($d in "bin", "lib", "share\extension") { New-Item -ItemType Directory -Path (Join-Path $OutDir $d) | Out-Null }
foreach ($f in $BinFiles) { Copy-Item (Join-Path $PgSource "bin\$f") (Join-Path $OutDir "bin") }
foreach ($f in $LibFiles) { Copy-Item (Join-Path $PgSource "lib\$f") (Join-Path $OutDir "lib") }
Get-ChildItem (Join-Path $PgSource "share") -File | Copy-Item -Destination (Join-Path $OutDir "share")
foreach ($d in $ShareDirs) { Copy-Item -Recurse (Join-Path $PgSource "share\$d") (Join-Path $OutDir "share\$d") }
foreach ($g in $ExtensionGlobs) {
    Get-ChildItem (Join-Path $PgSource "share\extension") -Filter $g | Copy-Item -Destination (Join-Path $OutDir "share\extension")
}

# --- build pgvector ---------------------------------------------------------
# Makefile.win installs into PGROOT, so build against a scratch PGROOT that
# has the source's headers and import library, then copy the results over.
$pgroot = Join-Path $Work "pgroot"
New-Item -ItemType Directory -Path $pgroot | Out-Null
Copy-Item -Recurse (Join-Path $PgSource "include") (Join-Path $pgroot "include")
Copy-Item -Recurse (Join-Path $PgSource "lib") (Join-Path $pgroot "lib")
New-Item -ItemType Directory -Path (Join-Path $pgroot "share\extension") | Out-Null
Write-Host "Building pgvector with MSVC..."
# vcvars64.bat looks vswhere up on PATH itself.
$cmd = "set `"PATH=%PATH%;$(Split-Path $vswhere)`" && call `"$vcvars`" >nul && cd /d `"$pgvectorSrc`" && set `"PGROOT=$pgroot`" && nmake /nologo /F Makefile.win && nmake /nologo /F Makefile.win install"
cmd /c $cmd | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Error "pgvector build failed (nmake exit $LASTEXITCODE)." }
Copy-Item (Join-Path $pgroot "lib\vector.dll") (Join-Path $OutDir "lib")
Get-ChildItem (Join-Path $pgroot "share\extension") -Filter "vector*" | Copy-Item -Destination (Join-Path $OutDir "share\extension")

# --- Visual C++ runtime -----------------------------------------------------
# postgres.exe and its DLLs need VCRUNTIME140/140_1 and MSVCP140 (the UCRT
# part ships with Windows 10+). A machine without the Visual C++
# Redistributable would not start the server, so the runtime is deployed
# app-locally next to postgres.exe, as Microsoft permits for these files.
# The newest runtime runs binaries built by older compilers.
$crt = Get-ChildItem (Join-Path $vsPath "VC\Redist\MSVC") -Directory |
    Where-Object { $_.Name -match '^\d+(\.\d+)+$' } |      # not v145 and the like
    Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Get-ChildItem (Join-Path $_.FullName "x64") -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (-not $crt) { Write-Error "No Visual C++ redistributable runtime under $vsPath\VC\Redist\MSVC." }
foreach ($f in "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll") {
    Copy-Item (Join-Path $crt.FullName $f) (Join-Path $OutDir "bin")
}
Write-Host "Visual C++ runtime: $($crt.FullName)"

# --- licenses ---------------------------------------------------------------
$lic = Join-Path $OutDir "licenses"
New-Item -ItemType Directory -Path $lic | Out-Null
foreach ($f in "server_license.txt", "commandlinetools_3rd_party_licenses.txt") {
    $p = Join-Path $PgSource $f
    if (Test-Path $p) { Copy-Item $p $lic }
}
Copy-Item (Join-Path $pgvectorSrc "LICENSE") (Join-Path $lic "pgvector_LICENSE.txt")

# --- check ------------------------------------------------------------------
$env:Path = "$env:SystemRoot\System32;$env:SystemRoot"   # nothing from other installs
$v = & (Join-Path $OutDir "bin\postgres.exe") --version
if ($LASTEXITCODE -ne 0) { Write-Error "The laid-out postgres.exe does not run." }
Remove-Item $Work -Recurse -Force
$size = (Get-ChildItem $OutDir -Recurse | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("Done: {0}, pgvector {1}, {2:N1} MB in {3}" -f $v, $PgvectorVersion, $size, $OutDir)
