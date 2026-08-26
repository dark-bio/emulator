# fetch-qemu-windows.ps1: populate launcher/binaries/ and launcher/qemu-libs/
# with a relocatable QEMU. The Windows counterpart of fetch-qemu-linux.sh and
# fetch-qemu-macos.sh, which share a qemu-common.sh this cannot use.
#
# The official Windows build (qemu.weilnetz.de) already ships every DLL
# qemu-system-*/qemu-img need side by side, with no Homebrew/apt-style
# absolute-path linking to undo. Unlike the other two platforms, this is just
# fetch, silent-install, and copy. DLLs go into launcher/qemu-libs/ (bundled as
# a Tauri resource, same as elsewhere) rather than next to the exes, and the
# launcher prepends that directory onto PATH before spawning (see platform.rs's
# `prepend_library_path`), which is how Windows' default DLL search order picks
# up a directory that isn't the exe's own.
#
# Only the QEMU system emulator matching the build host's own architecture is
# bundled, under the generic "qemu-system-guest" sidecar name; see
# qemu-common.sh for why. qemu-img is always bundled.
#
# Run from the emulator repo root:
#   pwsh .github/scripts/fetch-qemu-windows.ps1
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Pinned rather than "newest on the index page", so a Windows build is
# reproducible and every input to the installer is verified, the same way
# FIRMWARE_TAG pins the firmware. To bump: pick a build from
# https://qemu.weilnetz.de/w64/ and take the digest from its published .sha512.
#
# That digest comes from the same server as the installer, so it pins the
# artifact rather than vouching for it: from here on any change to the file is
# caught, but the file was taken on trust once. There is no upstream signature
# to check against, and the installer is not code-signed.
$QemuVersion = "20260811"
$QemuSha512 = "5bcf9eed634e8575a37b74f445af41a2fe4106da512d0c30c368301d4c105037fdfab40a5287367a28a957624cddebbc8c07e16c88ab6634f554cdf3d16bf543"

$repoRoot = Resolve-Path "$PSScriptRoot/../.."
$binDir = Join-Path $repoRoot "launcher/binaries"
$libsDir = Join-Path $repoRoot "launcher/qemu-libs"
New-Item -ItemType Directory -Force -Path $binDir, $libsDir | Out-Null

$triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
if (-not $triple) {
    throw "could not determine host target triple via 'rustc -vV'"
}

$installer = "qemu-w64-setup-$QemuVersion.exe"
$installerPath = Join-Path $env:TEMP $installer
Invoke-WebRequest -Uri "https://qemu.weilnetz.de/w64/$installer" -OutFile $installerPath

$actual = (Get-FileHash -Algorithm SHA512 -Path $installerPath).Hash
if ($actual -ne $QemuSha512.ToUpper()) {
    throw "checksum mismatch for ${installer}: expected $QemuSha512, got $actual"
}

# NSIS silent install; /D must be the last argument and unquoted-in-effect
# (no trailing backslash), per NSIS convention.
$installDir = Join-Path $env:TEMP "qemu-install"
Start-Process -FilePath $installerPath -ArgumentList "/S", "/D=$installDir" -Wait

$nativeQemu = switch -Regex ($triple) {
    "^aarch64-" { "qemu-system-aarch64.exe" }
    "^x86_64-"  { "qemu-system-x86_64.exe" }
    default     { throw "unsupported host architecture in triple $triple" }
}

$binaries = @{
    "qemu-system-guest" = $nativeQemu
    "qemu-img"          = "qemu-img.exe"
}
foreach ($name in $binaries.Keys) {
    $src = Join-Path $installDir $binaries[$name]
    $dest = Join-Path $binDir "$name-$triple.exe"
    Copy-Item $src $dest -Force
}

# Copy every DLL rather than working out which of the exes needs which subset:
# they're small relative to the qemu-system-* binaries, and this avoids
# re-deriving Windows' own dependency graph.
Get-ChildItem (Join-Path $installDir "*.dll") | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $libsDir $_.Name) -Force
}

# The firmware/BIOS files, whose subdirectory within the installer varies by
# version, hence the recursive search. The 5MB cap excludes the ~64MB ARM UEFI
# blobs (edk2-arm-{code,vars}.fd) if present, which a direct kernel boot never
# uses. All land in the same libsDir the launcher points -L at (see qemu.rs's
# `spawn_qemu`).
#
# Filtering by extension after an unrestricted -Recurse, rather than via
# -Include, is deliberate: Get-ChildItem -Recurse -Include is unreliable unless
# -Path itself ends in a wildcard, and a wrong pattern here matches nothing
# without erroring.
$firmwareExtensions = ".bin", ".rom", ".fd", ".dtb"
$firmwareFiles = Get-ChildItem $installDir -Recurse -File |
    Where-Object { $_.Extension -in $firmwareExtensions -and $_.Length -lt 5MB }
foreach ($file in $firmwareFiles) {
    Copy-Item $file.FullName (Join-Path $libsDir $file.Name) -Force
}

# Asserted per guest arch, since a silent miss here only surfaces later as an
# opaque QEMU firmware error on a machine that has no QEMU to fall back to.
# efi-virtio.rom is the option ROM every virtio-pci device carries, so both
# guests need it.
$requiredFirmware = @("efi-virtio.rom")
if ($nativeQemu -eq "qemu-system-x86_64.exe") {
    $requiredFirmware += "bios-256k.bin", "vgabios-stdvga.bin"
}
foreach ($name in $requiredFirmware) {
    if (-not ($firmwareFiles | Where-Object { $_.Name -eq $name })) {
        throw "$name not found under $installDir; did the QEMU installer layout change?"
    }
}

Write-Host "populated $binDir (triple $triple) and $libsDir"
Get-ChildItem $binDir, $libsDir | Format-Table Name, Length
