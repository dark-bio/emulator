# Updates only the current user's PATH during installation or uninstallation.
# The installer extracts this helper to a temporary directory, not the app folder.
param(
    [Parameter(Mandatory = $true)]
    [string]$Directory,
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'

$key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
try {
    # Keep expandable entries and the registry value's type intact
    $path = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $kind = if ($null -eq $path) { [Microsoft.Win32.RegistryValueKind]::ExpandString } else { $key.GetValueKind('Path') }
    $entries = if ([string]::IsNullOrEmpty($path)) { @() } else { @($path.Split(';')) }
    $remaining = @($entries | Where-Object {
        [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\') -ine $Directory.TrimEnd('\')
    })
    if ($Remove) {
        if ($remaining.Count -eq $entries.Count) { exit 0 }
        if ($remaining.Count -eq 0) {
            $key.DeleteValue('Path', $false)
            exit 0
        }
        $updated = $remaining -join ';'
    }
    else {
        if ($remaining.Count -lt $entries.Count) { exit 0 }
        $updated = (@($Directory) + $entries) -join ';'
    }
    $key.SetValue('Path', $updated, $kind)
}
finally {
    $key.Dispose()
}
