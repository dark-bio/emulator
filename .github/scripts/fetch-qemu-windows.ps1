# fetch-qemu-windows.ps1: the Windows counterpart of fetch-qemu-linux.sh and
# fetch-qemu-macos.sh, which share a qemu-common.sh this cannot use.
#
# The official Windows build already ships every DLL side by side, with no
# absolute-path linking to undo, so this is just fetch, silent-install, and
# copy. DLLs still go into launcher/qemu-libs/ rather than next to the exes, to
# match the other two platforms.
#
# Only the host's own architecture is bundled, as the generic
# "qemu-system-guest" sidecar; qemu-img is always bundled.
#
#   pwsh .github/scripts/fetch-qemu-windows.ps1
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Pinned rather than "newest on the index page", so a Windows build is
# reproducible. To bump, take a build and its published .sha512 from
# https://qemu.weilnetz.de/w64/.
#
# The digest comes from the same server as the installer, so it pins the
# artifact rather than vouching for it. There is no upstream signature to check
# against, and the installer is not code-signed.
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

# Every DLL, rather than re-deriving Windows' own dependency graph to find the
# subset the exes need. They are small next to the qemu-system-* binaries.
Get-ChildItem (Join-Path $installDir "*.dll") | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $libsDir $_.Name) -Force
}

# The firmware/BIOS files, whose subdirectory within the installer varies by
# version, hence the recursive search. The size cap excludes the ARM UEFI blobs,
# which a direct kernel boot never uses.
#
# Filtering by extension after an unrestricted -Recurse rather than via -Include
# is deliberate: -Recurse -Include is unreliable unless -Path itself ends in a
# wildcard, and a wrong pattern matches nothing without erroring.
$firmwareExtensions = ".bin", ".rom", ".fd", ".dtb"
$firmwareFiles = Get-ChildItem $installDir -Recurse -File |
    Where-Object { $_.Extension -in $firmwareExtensions -and $_.Length -lt 5MB }
foreach ($file in $firmwareFiles) {
    Copy-Item $file.FullName (Join-Path $libsDir $file.Name) -Force
}

# A silent miss only surfaces later as an opaque QEMU firmware error, on a
# machine with no QEMU to fall back to. efi-virtio.rom is carried by every
# virtio-pci device.
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
