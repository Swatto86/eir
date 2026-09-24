#Requires -Version 5.1
# WebView2 150+ ignores environment overrides for remote debugging on elevated
# hosts, and GitHub's Windows runners are elevated. Keep the machine-policy
# override restricted to eir.exe on disposable GitHub runners only — never run
# this outside CI, and never touch a policy value that isn't this run's own.
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'Only disposable GitHub runners may use this override' }
$root = Split-Path -Parent $PSScriptRoot
$profileDir = Join-Path $env:RUNNER_TEMP ('eir-webview-' + [guid]::NewGuid().ToString())
$base = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge\WebView2'
$values = @{
    AdditionalBrowserArguments = '--remote-debugging-port=0'
    UserDataFolder = $profileDir
}
$created = @()
try {
    foreach ($name in $values.Keys) {
        $key = Join-Path $base $name
        New-Item -Path $key -Force | Out-Null
        if ($null -ne (Get-Item $key).GetValue('eir.exe')) { throw 'Existing Eir WebView2 policy must not be overwritten' }
        New-ItemProperty -Path $key -Name 'eir.exe' -Value $values[$name] -PropertyType String | Out-Null
        $created += $key
    }
    & (Join-Path $root 'scripts\run-e2e.ps1')
} finally {
    foreach ($key in $created) { Remove-ItemProperty -Path $key -Name 'eir.exe' -ErrorAction SilentlyContinue }
    if (Test-Path $profileDir) { Remove-Item -LiteralPath $profileDir -Recurse -Force -ErrorAction SilentlyContinue }
}
# No explicit exit here: run-e2e.ps1 signals failure by throwing (never by
# returning with a nonzero $LASTEXITCODE — see its own `throw "the e2e suite
# failed ..."`), and an uncaught terminating error already exits this script
# non-zero by default, so $created's cleanup above still runs via `finally`.
