#Requires -Version 5.1
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$buildScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'build-portable.ps1') -Raw
$smokeScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-smoke.ps1') -Raw
$launchScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-launch.cmd') -Raw
if ($buildScript -notmatch '(?m)^Compress=0\r?$') {
    throw 'Portable packaging must not recompress the already-compressed WebView2 CAB.'
}
if ($buildScript -notmatch '\[int\]\$IExpressTimeoutSeconds' -or
    $buildScript -notmatch '\.WaitForExit\(\$IExpressTimeoutSeconds \* 1000\)' -or
    $buildScript -notmatch 'taskkill\.exe.*?/PID.*?/T.*?/F') {
    throw 'Portable packaging does not bound and terminate the full IExpress process tree.'
}
if ($buildScript -notmatch '~\$outputBase\.CAB' -or
    $buildScript -notmatch '~\$outputBase\.DDF') {
    throw 'Portable packaging does not clean stale IExpress cabinet state.'
}
if ($launchScript.IndexOf('cd /d "%TEMP%"', [StringComparison]::OrdinalIgnoreCase) -lt 0 -or
    $launchScript.IndexOf('cd /d "%TEMP%"', [StringComparison]::OrdinalIgnoreCase) -gt
        $launchScript.IndexOf('portable-run.ps1', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Portable launcher must leave its extraction directory before starting the runner.'
}
foreach ($fragment in @(
    '[Environment]::GetEnvironmentVariable(''RUST_LOG'', ''Process'')',
    '$env:RUST_LOG = ''info''',
    '[Environment]::SetEnvironmentVariable(''RUST_LOG'', $ambientRustLog, ''Process'')'
)) {
    if (-not $smokeScript.Contains($fragment)) {
        throw 'Portable smoke does not force its required info marker and restore ambient RUST_LOG.'
    }
}

$root = Join-Path ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\target'))) `
    "portable-runner-selftest-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $root | Out-Null
try {
    & (Join-Path $PSScriptRoot 'portable-run.ps1') -SelfTest -SelfTestRoot $root
}
finally {
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}
