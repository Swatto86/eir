#Requires -Version 5.1
<#
.SYNOPSIS
    Full repository gate: version agreement, formatting, lints, and the test suite.
.DESCRIPTION
    Mirrors the source and portable-binary checks .github/workflows/ci.yml runs on
    windows-latest, minus the NSIS bundle (use `cargo tauri build` in eir-ui for that).
    Run before every commit; a green run here is what CI should confirm, not discover.
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

# Version skew between the manifests and Cargo.lock ships mismatched binaries, so it
# gates first — it is also the cheapest check.
Invoke-Step 'check-versions' { & (Join-Path $PSScriptRoot 'check-versions.ps1') }
Invoke-Step 'installer hooks' { & (Join-Path $PSScriptRoot 'test-installer-hooks.ps1') }
Invoke-Step 'release gates' { & (Join-Path $PSScriptRoot 'test-release-gates.ps1') }
Invoke-Step 'portable runner' { & (Join-Path $PSScriptRoot 'test-portable-runner.ps1') }
Invoke-Step 'node --check ui/main.js' { node --check ui/main.js }
Invoke-Step 'cargo fmt --check' { cargo fmt --all -- --check }
Invoke-Step 'stage service binary' { & eir-ui/build-svc.ps1 }
Invoke-Step 'cargo clippy' { cargo clippy --locked --workspace --all-targets -- -D warnings }
Invoke-Step 'cargo test' { cargo test --locked --workspace --all-targets }
Invoke-Step 'cargo build --release' { cargo build --locked --workspace --release }
Invoke-Step 'portable imports' {
    & (Join-Path $PSScriptRoot 'check-portable-imports.ps1') `
        -EirExe target/release/eir.exe -ServiceExe target/release/eir-svc.exe
}
Invoke-Step 'cargo deny advisories' { cargo deny -t x86_64-pc-windows-msvc check advisories }

Write-Host 'verify OK' -ForegroundColor Green
