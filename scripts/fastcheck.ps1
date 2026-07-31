#Requires -Version 5.1
<#
.SYNOPSIS
    Fast per-edit check: formatting and type/lint correctness, no test run.
.DESCRIPTION
    Deliberately excludes `cargo test` and the Tauri bundle so it stays quick enough to
    run after every edit. scripts/verify.ps1 is the full gate before a commit.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Invoke-Step {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Step)
    Write-Host "==> $Name" -ForegroundColor Cyan
    & $Step
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $Name (exit $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Invoke-Step 'node --check ui/main.js' { node --check ui/main.js }
Invoke-Step 'cargo fmt --check' { cargo fmt --all -- --check }
if (-not (Test-Path 'eir-ui/bin/eir-svc.exe')) {
    Invoke-Step 'stage service binary' { & eir-ui/build-svc.ps1 }
}
Invoke-Step 'cargo clippy' { cargo clippy --locked --workspace --all-targets -- -D warnings }

Write-Host 'fastcheck OK' -ForegroundColor Green
