#Requires -Version 5.1
<#
.SYNOPSIS
    Full repository gate: version agreement, formatting, lints, and the test suite.
.DESCRIPTION
    Mirrors the checks .github/workflows/ci.yml runs on windows-latest, minus the release
    build and NSIS bundle (use `cargo tauri build` in eir-ui for that). Run before every
    commit; a green run here is what CI should confirm, not discover.
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

# Version skew between the four manifests ships a UI that cannot talk to its service,
# so it gates first — it is also the cheapest check.
Invoke-Step 'check-versions' { & (Join-Path $PSScriptRoot 'check-versions.ps1') }
Invoke-Step 'cargo fmt --check' { cargo fmt --all -- --check }
Invoke-Step 'cargo clippy' { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'cargo test' { cargo test --workspace --all-targets }

Write-Host 'verify OK' -ForegroundColor Green
