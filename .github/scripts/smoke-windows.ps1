# smoke-windows.ps1: the Windows half of smoke-unix.sh; see that script's header.
#
# Calls the packaged command entry point and captures stdout and stderr separately.
# The command waits for readiness while the emulator it starts keeps running.
#
#   pwsh .github/scripts/smoke-windows.ps1 -Executable <path> [-Arguments ...]
#
# Env: SMOKE_TIMEOUT (seconds), SMOKE_LOG.
param(
    [Parameter(Mandatory = $true)]
    [string]$Executable,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
)
$ErrorActionPreference = "Stop"

$timeout = if ($env:SMOKE_TIMEOUT) { [int]$env:SMOKE_TIMEOUT } else { 300 }
$log     = if ($env:SMOKE_LOG)     { $env:SMOKE_LOG }           else { "smoke.log" }
$events  = [IO.Path]::ChangeExtension($log, ".events.log")

if (-not (Test-Path $Executable)) {
    throw "$Executable does not exist"
}
$wrapper = Join-Path (Split-Path $Executable) "bin/ark-emulator.cmd"
if (-not (Test-Path $wrapper)) {
    throw "$wrapper does not exist"
}

function Write-Log {
    foreach ($file in @($log, $events)) {
        Write-Host "----- $file -----"
        if (Test-Path $file) { Write-Host (Get-Content $file -Raw) }
    }
    Write-Host "----- end of logs -----"
}

# Runs one command and records its streams before inspecting the exit code.
function Invoke-Emulator([string[]]$commandArgs, [switch]$Append) {
    & $wrapper @commandArgs 1> "$log.part" 2> "$events.part"
    $status = $LASTEXITCODE
    foreach ($pair in @(@($log, "$log.part"), @($events, "$events.part"))) {
        if (Test-Path $pair[1]) {
            if ($Append) { Get-Content $pair[1] | Add-Content $pair[0] }
            else { Move-Item $pair[1] $pair[0] -Force }
            Remove-Item $pair[1] -ErrorAction SilentlyContinue
        }
    }
    return $status
}

# The emulator outlives this script, so a failure past the start has to take it
# down on the way out. Only one this run booted: a start that reported an
# emulator already running leaves it to whoever started it.
$port = ""
$started = ""
try {
    New-Item -ItemType File -Force -Path $log, $events | Out-Null

    # Desktop launches must still open without a console window
    $bytes = [IO.File]::ReadAllBytes((Resolve-Path $Executable))
    $pe = [BitConverter]::ToInt32($bytes, 0x3c)
    $subsystem = [BitConverter]::ToUInt16($bytes, $pe + 24 + 68)
    if ($subsystem -ne 2) { throw "the desktop executable is not a GUI application" }

    $status = Invoke-Emulator @("--json", "list", "--bogus")
    if ($status -ne 2) { throw "a usage error did not return exit code 2" }
    $document = Get-Content $log -Raw | ConvertFrom-Json
    if ($document.error.code -ne "usage") { throw "a usage error lost its JSON result" }

    $helpText = & $wrapper help start | Out-String
    if ($LASTEXITCODE -ne 0 -or $helpText -notmatch 'Requires:') {
        throw "help could not be read through a PowerShell pipeline"
    }

    Write-Host "starting $Executable"
    $startArgs = @("start", "--no-input", "--json", "--timeout", "$timeout") + $Arguments
    $status = Invoke-Emulator $startArgs

    # The document is read before the status is judged, since a start that
    # timed out still names the emulator it left booting, and that one has to
    # be stopped on the way out.
    $document = $null
    try { $document = Get-Content $log -Raw | ConvertFrom-Json } catch {}
    if ($document) {
        $port = "$($document.port)"
        $ready = "$($document.ready)".ToLower()
        $started = "$($document.started)".ToLower()
    }
    if ($status -ne 0) {
        Write-Host "start exited with status $status"
        Write-Log
        exit 1
    }
    if ($ready -ne "true" -or $started -ne "true" -or -not $port) {
        Write-Host "start answered started=$started ready=$ready port=$port"
        Write-Log
        exit 1
    }
    Write-Host "the device on port $port accepts clients"

    Write-Host "stopping the emulator on port $port"
    $stopArgs = @("stop", "emulator:$port", "--no-input", "--json", "--timeout", "$timeout")
    $status = Invoke-Emulator $stopArgs -Append
    if ($status -ne 0) {
        Write-Host "stop exited with status $status"
        Write-Log
        exit 1
    }
    $port = ""

    Write-Log
    Write-Host "the emulator booted and shut down"
}
finally {
    if ($port -and $started -eq "true") {
        Invoke-Emulator @("stop", "emulator:$port", "--no-input") -Append | Out-Null
    }
}
