# fetch-qemu-windows.ps1: the Windows counterpart of fetch-qemu-linux.sh and
# fetch-qemu-macos.sh, which share a qemu-common.sh this cannot use.
#
# The Windows distribution supplies DLLs beside its executables. Follow the
# imports of the two bundled tools with Visual Studio's dumpbin, and copy their
# dependencies into launcher/qemu-libs/ to match the other platforms.
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
# A repeated fetch must not retain DLLs or ROMs from a broader installation
foreach ($dir in @($binDir, $libsDir)) {
    if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
}
New-Item -ItemType Directory -Force -Path $binDir, $libsDir | Out-Null

$triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
if (-not $triple) {
    throw "could not determine host target triple via 'rustc -vV'"
}

# The Visual Studio tools are installed on the runner but may not be on PATH
$dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
if (-not $dumpbin) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio/Installer/vswhere.exe"
    if (Test-Path $vswhere) {
        $dumpbin = & $vswhere -latest -products '*' `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
    }
}
if (-not $dumpbin) {
    throw "dumpbin.exe not found; install the Visual Studio C++ build tools"
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
    "^aarch64-" { $arch = "arm64"; "qemu-system-aarch64.exe" }
    "^x86_64-"  { $arch = "amd64"; "qemu-system-x86_64.exe" }
    default     { throw "unsupported host architecture in triple $triple" }
}

# Copies imports recursively, including delay imports reported by dumpbin.
# DLLs absent from the distribution must resolve to Windows or its API sets.
# Runtime-loaded graphics backends are unused by the headless QEMU guest.
function Copy-Dependencies([string]$Binary) {
    $imports = & $dumpbin /nologo /dependents $Binary
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin failed for $Binary"
    }
    foreach ($line in $imports) {
        if ($line -notmatch '^\s+(\S+\.dll)\s*$') { continue }
        $name = $Matches[1]
        $src = Join-Path $installDir $name
        $dest = Join-Path $libsDir $name
        if (Test-Path $dest) { continue }

        if (Test-Path $src) {
            Copy-Item $src $dest
            Copy-Dependencies $src
        } elseif ($name -notmatch '^(api|ext)-ms-win-' -and
                  -not (Test-Path (Join-Path "$env:WINDIR/System32" $name))) {
            throw "$name imported by $Binary is missing from QEMU and Windows"
        }
    }
}

$binaries = @{
    "qemu-system-guest" = $nativeQemu
    "qemu-img"          = "qemu-img.exe"
}
foreach ($name in $binaries.Keys) {
    $src = Join-Path $installDir $binaries[$name]
    $dest = Join-Path $binDir "$name-$triple.exe"
    Copy-Item $src $dest -Force
    Copy-Dependencies $src
}

# ROM locations within the Windows distribution vary by version
$firmwareFiles = Get-ChildItem $installDir -Recurse -File
$requiredFirmware = Get-Content (Join-Path $repoRoot ".github/packaging/qemu/$arch.roms")
foreach ($name in $requiredFirmware) {
    $file = $firmwareFiles | Where-Object { $_.Name -eq $name } | Select-Object -First 1
    if (-not $file) {
        throw "$name not found under $installDir; did the QEMU installer layout change?"
    }
    Copy-Item $file.FullName (Join-Path $libsDir $name)
}

Write-Host "populated $binDir (triple $triple) and $libsDir"
Get-ChildItem $binDir, $libsDir | Format-Table Name, Length
