# Checks installer PATH changes, command lookup, reinstallation and uninstall cleanup.
# Run only on a disposable Windows runner; this installs the packaged application.
param(
    [Parameter(Mandatory = $true)]
    [string]$Installer
)
$ErrorActionPreference = 'Stop'
$Installer = (Resolve-Path $Installer).Path
$installDir = Join-Path $env:TEMP ('Ark Emulator café ' + [guid]::NewGuid())
$commandDir = Join-Path $installDir 'bin'
$uninstaller = Join-Path $installDir 'uninstall.exe'
$key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
$raw = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
$originalPath = $key.GetValue('Path', $null, $raw)
$originalKind = if ($null -eq $originalPath) { $null } else { $key.GetValueKind('Path') }
$originalProcessPath = $env:Path
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$shells = @((Get-Command powershell.exe).Source, (Get-Command pwsh.exe).Source)

# Runs an installer or uninstaller to completion, including its child processes.
function Invoke-Setup([string]$File, [string]$CommandLine) {
    $process = Start-Process -FilePath $File -ArgumentList $CommandLine -PassThru -Wait
    try {
        if ($process.ExitCode -ne 0) { throw "$File exited with $($process.ExitCode)" }
    }
    finally { $process.Dispose() }
}

try {
    # Exceed NSIS's default string limit and retain unexpanded environment references
    $seed = (1..100 | ForEach-Object { "%USERPROFILE%\ark-path-test-$_" }) -join ';'
    $key.SetValue('Path', $seed, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    $expected = "$commandDir;$seed"

    for ($attempt = 0; $attempt -lt 2; $attempt++) {
        # NSIS requires /D to be last, with the path unquoted even when it contains spaces
        Invoke-Setup $Installer "/S /D=$installDir"
        if ($key.GetValue('Path', $null, $raw) -cne $expected) {
            throw 'installation did not preserve the user PATH or added duplicate entries'
        }
        if ($key.GetValueKind('Path') -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
            throw 'installation changed the PATH registry type'
        }
    }

    # Rebuild the environment as a new terminal would after installation
    $env:Path = $machinePath + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
    $command = Get-Command ark-emulator -CommandType Application
    if ($command.Source -ine (Join-Path $commandDir 'ark-emulator.cmd')) {
        throw "ark-emulator resolved to $($command.Source)"
    }
    foreach ($shell in $shells) {
        $help = & $shell -NoProfile -NonInteractive -Command 'ark-emulator help start; exit $LASTEXITCODE' | Out-String
        if ($LASTEXITCODE -ne 0 -or $help -notmatch 'Requires:') {
            throw "the global command failed in $shell"
        }
    }
    $help = & $env:ComSpec /d /c 'ark-emulator help start' | Out-String
    if ($LASTEXITCODE -ne 0 -or $help -notmatch 'Requires:') {
        throw 'the global command failed in Command Prompt'
    }

    # Uninstall must preserve entries added after installation as well
    $after = "$seed;%USERPROFILE%\added-after-install"
    $key.SetValue('Path', "$commandDir;$after", [Microsoft.Win32.RegistryValueKind]::ExpandString)
    Invoke-Setup $uninstaller "/S _?=$installDir"
    if ($key.GetValue('Path', $null, $raw) -cne $after) {
        throw 'uninstall changed unrelated PATH entries or left the command directory behind'
    }
    if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -cne $machinePath) {
        throw 'the installer changed the machine PATH'
    }
    if (Test-Path (Join-Path $commandDir 'ark-emulator.cmd')) {
        throw 'uninstall left the command launcher behind'
    }
    Write-Host 'Installer PATH setup, reinstall and uninstall passed'
}
finally {
    try {
        if (Test-Path $uninstaller) { Invoke-Setup $uninstaller "/S _?=$installDir" }
    }
    finally {
        if ($null -eq $originalPath) { $key.DeleteValue('Path', $false) }
        else { $key.SetValue('Path', $originalPath, $originalKind) }
        $key.Dispose()
        $env:Path = $originalProcessPath
    }
}
