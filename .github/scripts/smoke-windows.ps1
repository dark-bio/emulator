# smoke-windows.ps1: launch a built emulator and wait for the emulated device
# to finish booting, for both CI and local development. The Windows half of
# smoke-unix.sh; see that script's header for what this check is for and why
# the boot marker is matched the way it is.
#
# Two log files rather than one, because Start-Process refuses to redirect
# stdout and stderr to the same path. The split is not arbitrary: QEMU's
# -serial stdio puts the guest console on stdout while the launcher's own
# "[launcher] ..." diagnostics go to stderr, so both are needed to explain a
# failure and both are matched against.
#
# A release build is linked as a GUI app and has no console of its own, but
# CREATE_NO_WINDOW only suppresses the console window, not the standard
# handles, so QEMU still inherits the redirected ones from the launcher.
#
# Exits non-zero if the process dies before the marker appears, if the marker
# never appears within the timeout, or if the service reports failure.
#
#   pwsh .github/scripts/smoke-windows.ps1 -Executable <path> [-Arguments ...]
#
# Env: SMOKE_TIMEOUT (seconds, default 60), SMOKE_MARKER, SMOKE_LOG.
param(
    [Parameter(Mandatory = $true)]
    [string]$Executable,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
)
$ErrorActionPreference = "Stop"

$timeout = if ($env:SMOKE_TIMEOUT) { [int]$env:SMOKE_TIMEOUT } else { 60 }
$marker  = if ($env:SMOKE_MARKER)  { $env:SMOKE_MARKER }        else { "Starting runcore" }
$log     = if ($env:SMOKE_LOG)     { $env:SMOKE_LOG }           else { "smoke.log" }
$errLog  = [IO.Path]::ChangeExtension($log, ".err.log")

if (-not (Test-Path $Executable)) {
    throw "$Executable does not exist"
}

# Strips ANSI colour and cursor-positioning escapes plus carriage returns,
# then reattaches any status bracket left stranded at the start of a line to
# the message above it, so a marker and its status share one line. Matching
# across lines instead would let a later service's "[ ok ]" satisfy an
# earlier service that never reported one.
# Reads a file the launcher still holds open for writing. Get-Content would
# fail on the sharing violation, and swallowing that error would look exactly
# like a guest that never printed anything.
function Read-SharedFile([string]$path) {
    if (-not (Test-Path $path)) { return "" }
    $stream = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try {
        $reader = New-Object IO.StreamReader($stream)
        return $reader.ReadToEnd()
    }
    finally {
        $stream.Dispose()
    }
}

function Get-PlainLog {
    $text = ""
    foreach ($file in @($log, $errLog)) {
        $text += (Read-SharedFile $file)
        $text += "`n"
    }
    $text = $text -replace "\x1B\[[0-9;?]*[a-zA-Z]", ""
    $text = $text -replace "\x1B[()][A-B0-9]", ""
    $text = $text -replace "`r", ""
    return $text -replace "`n[ \t]*(\[ *(?:ok|!!|oops) *\])", ' $1'
}

# Whether the marker and the given status share a line. SMOKE_MARKER is used
# as a regular expression, not a literal.
function Test-Marker([string]$status) {
    foreach ($line in (Get-PlainLog) -split "`n") {
        if ($line -match "$marker.*\[ *$status *\]") { return $true }
    }
    return $false
}

function Write-Log {
    Write-Host "----- $log -----"
    Write-Host (Get-PlainLog)
    Write-Host "----- end of $log -----"
}

New-Item -ItemType File -Force -Path $log, $errLog | Out-Null

$startArgs = @{
    FilePath               = $Executable
    PassThru               = $true
    RedirectStandardOutput = $log
    RedirectStandardError  = $errLog
}
if ($Arguments) {
    $startArgs.ArgumentList = $Arguments
}

Write-Host "launching $Executable $Arguments"
$proc = Start-Process @startArgs

# The launcher ties QEMU's lifetime to its own via a Job Object, so killing
# the launcher is enough to tear the guest down too.
try {
    $deadline = (Get-Date).AddSeconds($timeout)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            Write-Host "the emulator exited with status $($proc.ExitCode) before reaching the marker"
            Write-Log
            exit 1
        }
        if (Test-Marker "ok") {
            Write-Host "matched `"$marker ... [ ok ]`", the device booted"
            Write-Log
            exit 0
        }
        if (Test-Marker "!!") {
            Write-Host "`"$marker`" reported failure"
            Write-Log
            exit 1
        }
        Start-Sleep -Seconds 1
    }

    if ((Get-PlainLog) -match $marker) {
        Write-Host "saw `"$marker`" but no bracketed status within ${timeout}s;"
        Write-Host "the marker regex may need adjusting for how the status was laid out"
    } else {
        Write-Host "no `"$marker`" within ${timeout}s"
    }
    Write-Log
    exit 1
}
finally {
    if (-not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit(5000) | Out-Null
    }
}
