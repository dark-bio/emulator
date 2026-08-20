# fetch-qemu-windows.ps1: populate launcher/binaries/ and launcher/qemu-libs/
# with a relocatable QEMU, for both CI and local development.
#
# The official Windows build (qemu.weilnetz.de) already ships every DLL
# qemu-system-*/qemu-img need side by side, with no Homebrew/apt-style
# absolute-path linking to undo. Unlike the macOS/Linux legs, this is just
# fetch, silent-install, and copy. DLLs go into launcher/qemu-libs/ (bundled
# as a Tauri resource, same as the other two platforms) rather than next to
# the exes, and the launcher prepends that directory onto PATH before
# spawning (see main.rs's `apply_library_path`), which is how Windows'
# default DLL search order picks up a directory that isn't the exe's own.
#
# Only the QEMU system emulator matching the build host's own architecture is
# bundled, as the generic "qemu-system-guest" sidecar name (which real
# qemu-system-*.exe that is depends on the host). Bundling both guest
# architectures would double the installer size for no benefit to most
# users; a source build still gets both via a PATH-installed QEMU (see
# main.rs's `spawn_qemu`). qemu-img has no per-arch variant and is always
# bundled.
#
# Run from the emulator repo root:
#   pwsh .github/scripts/fetch-qemu-windows.ps1
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repoRoot = Resolve-Path "$PSScriptRoot/../.."
$binDir = Join-Path $repoRoot "launcher/binaries"
$libsDir = Join-Path $repoRoot "launcher/qemu-libs"
New-Item -ItemType Directory -Force -Path $binDir, $libsDir | Out-Null

$triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
if (-not $triple) {
    throw "could not determine host target triple via 'rustc -vV'"
}

$indexUrl = "https://qemu.weilnetz.de/w64/"
$index = Invoke-WebRequest -Uri $indexUrl -UseBasicParsing
$installer = $index.Links.href |
    Where-Object { $_ -match '^qemu-w64-setup-[\d.]+\.exe$' } |
    Sort-Object |
    Select-Object -Last 1
if (-not $installer) {
    throw "could not find a qemu-w64-setup-*.exe installer at $indexUrl"
}

$installerPath = Join-Path $env:TEMP $installer
Invoke-WebRequest -Uri "$indexUrl$installer" -OutFile $installerPath

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

# Copy every DLL rather than trying to work out which of the three exes
# needs which subset: they're small relative to the qemu-system-* binaries,
# and this avoids re-deriving Windows' own dependency graph. Also grab the
# firmware/BIOS files QEMU needs (e.g. bios-256k.bin for the x86_64 guest's
# q35 machine model, which SeaBIOS runs before a direct -kernel boot); their
# exact subdirectory within the installer varies by version, so search
# recursively rather than hardcoding a path. All land in the same libsDir the
# launcher points -L at (see main.rs's `spawn_qemu`); QEMU looks up specific
# filenames there and ignores everything else.
Get-ChildItem (Join-Path $installDir "*.dll") | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $libsDir $_.Name) -Force
}
# The 5MB cap excludes the ~64MB ARM UEFI blobs (edk2-arm-{code,vars}.fd) if
# present, unneeded since the arm64 virt board boots fine on a direct kernel
# boot with zero firmware files (confirmed separately) and would otherwise
# dominate the bundle size.
Get-ChildItem $installDir -Recurse -Include "*.bin", "*.rom", "*.fd", "*.dtb" |
    Where-Object { $_.Length -lt 5MB } |
    ForEach-Object {
        Copy-Item $_.FullName (Join-Path $libsDir $_.Name) -Force
    }

Write-Host "populated $binDir (triple $triple) and $libsDir"
Get-ChildItem $binDir, $libsDir | Format-Table Name, Length
