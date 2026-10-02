# Enclave - a clean Windows 11 virtual machine for the installer check
#
# Creates a VirtualBox VM that meets Windows 11's requirements (EFI, Secure
# Boot, TPM 2.0, 64 GB disk), installs Windows from the ISO unattended with
# a local account and the Guest Additions, and shares the folder with the
# Enclave installer read-only. Nothing of the development machine is in
# the VM: no Visual C++ runtime, no PostgreSQL, no NVIDIA driver, no GPU -
# the installer has to bring or download everything itself (ADR-0014,
# ADR-0024, ADR-0025), and the engine it picks is the CPU build.
#
# Usage (from enclave/, VirtualBox 7 installed):
#   powershell -File .\scripts\make-test-vm.ps1 -Iso D:\Win11_Russian_x64.iso
#
# Asks for the password of the VM's local account; it goes to VirtualBox in
# a file deleted right after. The VM starts in a window and installs
# itself (30-90 min). Remove it with:
#   VBoxManage unregistervm EnclaveClean --delete

param(
    [Parameter(Mandatory = $true)] [string] $Iso,
    [string] $Name         = "EnclaveClean",
    [int]    $MemoryMB     = 6144,
    [int]    $Cpus         = 4,
    [int]    $DiskGB       = 64,
    [string] $User         = "tester",
    [string] $InstallerDir = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if (-not $InstallerDir) { $InstallerDir = Join-Path $Root "target\release\bundle\nsis" }

$VBox = Join-Path $env:ProgramFiles "Oracle\VirtualBox\VBoxManage.exe"
if (-not (Test-Path $VBox)) { Write-Error "VirtualBox not found ($VBox). Install VirtualBox 7 first." }
if (-not (Test-Path $Iso)) { Write-Error "No ISO at $Iso." }
if (-not (Get-ChildItem $InstallerDir -Filter "*-setup.exe" -ErrorAction SilentlyContinue)) {
    Write-Error "No installer in $InstallerDir - run scripts\build-installer.ps1 first."
}

function VBox {
    & $VBox @args
    if ($LASTEXITCODE -ne 0) { Write-Error "VBoxManage $($args -join ' ') failed (exit $LASTEXITCODE)." }
}

if ((& $VBox list vms) -match "^`"$Name`"") {
    Write-Error "A VM named $Name already exists. Remove it (VBoxManage unregistervm $Name --delete) or pass -Name."
}

# Which edition: plain Pro if the ISO has it (a local account without
# tricks), else the first image. `detect` lists them as
#   ImageIndex4="Windows 11 Pro (10.0.26300.9457 / x64 / ru-RU)"
# - "Windows 11 Pro (" excludes Pro for Education / Workstations in any
# language, whose names continue after "Pro".
$detected = & $VBox unattended detect --iso=$Iso --machine-readable
$names = @{}
foreach ($line in $detected) {
    if ($line -match '^ImageIndex(\d+)="(.+)"') { $names[$Matches[1]] = $Matches[2] }
}
$index = 1
foreach ($k in ($names.Keys | Sort-Object { [int]$_ })) {
    if ($names[$k] -like "Windows 11 Pro (*") { $index = [int]$k; break }
}
$lang = ($detected | Select-String '^OSLanguages="([^"]+)"').Matches | ForEach-Object { $_.Groups[1].Value } | Select-Object -First 1
if (-not $lang) { $lang = "en-US" }
# One string, built here: an expression inside an argument (--locale=(…))
# is passed by PowerShell as two arguments, and VBoxManage then takes the
# second for another VM name.
$locale = $lang -replace '-', '_'
Write-Host "Windows image $index ($($names["$index"])), language $lang"

$password = Read-Host "Password for the VM's local account '$User'" -AsSecureString
$plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($password))
$pwFile = Join-Path $env:TEMP "enclave-vm-pw-$PID.txt"

try {
    VBox createvm --name $Name --ostype Windows11_64 --register
    $vmDir = Split-Path ((& $VBox showvminfo $Name --machinereadable | Select-String '^CfgFile="(.+)"').Matches[0].Groups[1].Value)

    VBox modifyvm $Name --memory $MemoryMB --cpus $Cpus --firmware efi --tpm-type 2.0 `
        --graphicscontroller vboxsvga --vram 128 --nic1 nat --clipboard-mode bidirectional `
        --drag-and-drop hosttoguest --audio-driver none --usb-xhci on
    VBox modifynvram $Name inituefivarstore
    VBox modifynvram $Name enrollmssignatures
    VBox modifynvram $Name enrollorclpk
    VBox modifynvram $Name secureboot --enable

    $disk = Join-Path $vmDir "$Name.vdi"
    VBox createmedium disk --filename $disk --size ($DiskGB * 1024) --variant Standard
    VBox storagectl $Name --name SATA --add sata --controller IntelAhci --portcount 2
    VBox storageattach $Name --storagectl SATA --port 0 --device 0 --type hdd --medium $disk

    # The installer folder, read-only, as drive E: in the guest.
    VBox sharedfolder add $Name --name installer --hostpath $InstallerDir --readonly --automount --auto-mount-point "E:"

    Set-Content -Path $pwFile -Value $plain -NoNewline -Encoding ascii
    VBox unattended install $Name --iso=$Iso --image-index=$index --user=$User --user-password-file=$pwFile `
        --full-user-name="Enclave Tester" "--locale=$locale" --install-additions `
        --hostname="$Name.local" --start-vm=gui
} finally {
    Remove-Item $pwFile -ErrorAction SilentlyContinue
    $plain = $null
}

Write-Host ""
Write-Host "The VM is installing Windows by itself; leave it alone until the desktop appears."
Write-Host "Then follow docs\clean-machine-check.md. The installer is on drive E: in the VM."
