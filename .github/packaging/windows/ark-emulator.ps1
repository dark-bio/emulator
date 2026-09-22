# Runs the adjacent GUI executable with its terminal streams intact, then
# returns its exit code. Compatible with Windows PowerShell 5.1 and PowerShell 7.
# PowerShell pipelines should invoke the executable directly to capture output.

$start = @{
    FilePath = Join-Path $PSScriptRoot 'ark-emulator.exe'
    WorkingDirectory = $PWD.ProviderPath
    NoNewWindow = $true
    PassThru = $true
    ErrorAction = 'Stop'
}
if (-not (Test-Path -LiteralPath $start.FilePath -PathType Leaf)) {
    $start.FilePath = Join-Path $PSScriptRoot 'Ark Emulator.exe'
}

# Windows splits a command line string. Double backslashes before quotes and
# the closing quote so empty arguments, embedded quotes and trailing slashes survive.
if ($args.Count) {
    $start.ArgumentList = ($args | ForEach-Object {
        '"' + (($_ -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
    }) -join ' '
}

$process = $null
try {
    # -NoNewWindow explicitly inherits standard handles, including on Windows PowerShell 5.1
    $process = Start-Process @start
    # Cache the handle before waiting so Windows PowerShell 5.1 retains the exit code
    $null = $process.Handle
    # Start-Process -Wait also waits for the emulator that a start leaves running
    $process.WaitForExit()
    exit $process.ExitCode
}
catch {
    $failure = @{ code = 'io'; message = "could not run Ark Emulator: $($_.Exception.Message)" }
    if ($args -contains '--json') {
        [Console]::Out.WriteLine((@{ error = $failure } | ConvertTo-Json -Depth 3))
        [Console]::Error.WriteLine(([ordered]@{ event = 'error'; error = $failure } | ConvertTo-Json -Depth 3 -Compress))
    }
    else {
        [Console]::Error.WriteLine("error[io]: $($failure.message)")
    }
    exit 1
}
finally {
    if ($null -ne $process) { $process.Dispose() }
}
