# smoke-windows.ps1: the Windows half of smoke-unix.sh; see that script's header.
#
# Start-Process rather than calling the executable, because a release build is
# linked as a GUI app and the shell does not wait for one of those. It also
# refuses to redirect stdout and stderr to the same path, which is why the
# result document and the events land in two files.
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

function Write-Log {
    foreach ($file in @($log, $events)) {
        Write-Host "----- $file -----"
        if (Test-Path $file) { Write-Host (Get-Content $file -Raw) }
    }
    Write-Host "----- end of logs -----"
}

# Runs one command of the emulator's own and answers with its exit code, having
# written its result and its events where Write-Log can find them.
function Invoke-Emulator([string[]]$commandArgs, [switch]$Append) {
    $startArgs = @{
        FilePath               = $Executable
        ArgumentList           = $commandArgs
        PassThru               = $true
        Wait                   = $true
        NoNewWindow            = $true
        RedirectStandardOutput = "$log.part"
        RedirectStandardError  = "$events.part"
    }
    $proc = Start-Process @startArgs
    foreach ($pair in @(@($log, "$log.part"), @($events, "$events.part"))) {
        if (Test-Path $pair[1]) {
            if ($Append) { Get-Content $pair[1] | Add-Content $pair[0] }
            else { Move-Item $pair[1] $pair[0] -Force }
            Remove-Item $pair[1] -ErrorAction SilentlyContinue
        }
    }
    return $proc.ExitCode
}

# The emulator outlives this script, so a failure past the start has to take it
# down on the way out.
$port = ""
try {
    New-Item -ItemType File -Force -Path $log, $events | Out-Null

    Write-Host "starting $Executable"
    $startArgs = @("start", "--no-input", "--json", "--timeout", "$timeout") + $Arguments
    $status = Invoke-Emulator $startArgs
    if ($status -ne 0) {
        Write-Host "start exited with status $status"
        Write-Log
        exit 1
    }

    # The document is ours and its shape is part of the contract, so matching it
    # is enough and keeps this script free of a JSON parser.
    $document = Get-Content $log -Raw
    $port = ([regex]::Match($document, '"port":\s*(\d+)')).Groups[1].Value
    $ready = ([regex]::Match($document, '"ready":\s*(\w+)')).Groups[1].Value
    $started = ([regex]::Match($document, '"started":\s*(\w+)')).Groups[1].Value

    if ($ready -ne "true" -or $started -ne "true" -or -not $port) {
        Write-Host "start answered started=$started ready=$ready port=$port"
        Write-Log
        exit 1
    }
    Write-Host "the device on port $port accepts clients"

    Write-Host "stopping the emulator on port $port"
    $stopArgs = @("stop", $port, "--no-input", "--json", "--timeout", "$timeout")
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
    if ($port) {
        Invoke-Emulator @("stop", $port, "--no-input") -Append | Out-Null
    }
}
