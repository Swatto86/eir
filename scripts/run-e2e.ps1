<#
.SYNOPSIS
  Build the debug app + service and run the WebdriverIO end-to-end suite.

.DESCRIPTION
  Locates (never silently downloads) `tauri-driver` and a WebView2-version-
  matched `msedgedriver`, installs the e2e suite's npm dependencies, and runs
  it against `target/debug/eir.exe` + `target/debug/eir-svc.exe`. The suite
  is fully isolated from any installed Eir — see e2e/service.ts.

.PARAMETER SkipBuild
  Skip `cargo build -p eir-ui -p eir-svc` and use whatever is already in
  target/debug (the suite itself refuses a stale or missing binary).
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Find-TauriDriver {
    $cmd = Get-Command tauri-driver -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidate = Join-Path $env:USERPROFILE '.cargo\bin\tauri-driver.exe'
    if (Test-Path $candidate) { return $candidate }
    throw "tauri-driver was not found on PATH or at $candidate. Install it with: cargo install tauri-driver --locked"
}

function Find-Msedgedriver {
    $key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $installed = (Get-ItemProperty $key -ErrorAction SilentlyContinue).pv
    if (-not $installed) { throw 'the WebView2 runtime is not installed, so the app cannot render at all' }
    Write-Host "  WebView2 runtime installed: $installed"

    $candidates = @(
        $env:EIR_E2E_MSEDGEDRIVER,
        (Join-Path $env:LOCALAPPDATA "Programs\msedgedriver\$installed\msedgedriver.exe"),
        (Join-Path $env:USERPROFILE 'bin\msedgedriver.exe')
    ) | Where-Object { $_ }

    foreach ($candidate in $candidates) {
        if (-not (Test-Path $candidate)) { continue }
        & (Join-Path $PSScriptRoot 'assert-microsoft-signature.ps1') -Path $candidate
        $have = (& $candidate --version) -replace '.*WebDriver\s+([\d.]+).*', '$1'
        if ($have -eq $installed) {
            Write-Host "  using $candidate ($have)"
            return $candidate
        }
        Write-Host "  $candidate is $have, need $installed — skipping" -ForegroundColor Yellow
    }

    throw (
        "No msedgedriver matching WebView2 $installed was found (checked: $($candidates -join ', ')). " +
        "This script does not download one automatically — fetch it yourself from " +
        "https://msedgedriver.microsoft.com/$installed/edgedriver_win64.zip, verify its Microsoft " +
        "signature (scripts/assert-microsoft-signature.ps1), and set the EIR_E2E_MSEDGEDRIVER " +
        "environment variable to it."
    )
}

Write-Host '== tauri-driver ==' -ForegroundColor Cyan
$tauriDriver = Find-TauriDriver
Write-Host "  using $tauriDriver"

Write-Host '== msedgedriver ==' -ForegroundColor Cyan
$env:EIR_E2E_MSEDGEDRIVER = Find-Msedgedriver

if (-not $SkipBuild) {
    Write-Host '== cargo build -p eir-ui -p eir-svc ==' -ForegroundColor Cyan
    Push-Location $root
    try {
        cargo build --locked -p eir-ui -p eir-svc
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        Pop-Location
    }
}

Write-Host '== npm install ==' -ForegroundColor Cyan
Push-Location (Join-Path $root 'e2e')
try {
    if (Test-Path 'package-lock.json') {
        npm ci --silent
    } else {
        npm install --silent
    }
    if ($LASTEXITCODE -ne 0) { throw 'npm install failed' }

    Write-Host '== wdio ==' -ForegroundColor Cyan
    npx wdio run wdio.conf.ts
    if ($LASTEXITCODE -ne 0) { throw "the e2e suite failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Host 'e2e OK' -ForegroundColor Green
