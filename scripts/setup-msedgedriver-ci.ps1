<#
.SYNOPSIS
  CI-only: fetch a WebView2-version-matched msedgedriver for the e2e suite.

.DESCRIPTION
  GitHub's windows-latest runners ship Edge/WebView2 but no msedgedriver, and
  scripts/run-e2e.ps1's Find-Msedgedriver deliberately never downloads one
  (see its own comments) — that's the right behaviour on a developer machine,
  but leaves CI with nothing to find. This script is the one place that
  downloads: it reads the installed WebView2 version from the same registry
  key Find-Msedgedriver uses, fetches the matching driver from the official
  msedgedriver.microsoft.com URL (mirroring scripts/setup-e2e.ps1 in the
  ComputeQuiet/SwatPulse reference harnesses), verifies its Microsoft
  signature, and publishes EIR_E2E_MSEDGEDRIVER via $GITHUB_ENV so
  Find-Msedgedriver picks it up (and re-verifies it) like any other run.

  Refuses to run outside GitHub Actions.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'This script only runs on CI (GITHUB_ACTIONS != true).' }
if (-not $env:RUNNER_TEMP) { throw 'RUNNER_TEMP is not set — expected a GitHub Actions runner.' }
if (-not $env:GITHUB_ENV) { throw 'GITHUB_ENV is not set — expected a GitHub Actions runner.' }

$key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$version = (Get-ItemProperty $key -ErrorAction SilentlyContinue).pv
if (-not $version) { throw 'the WebView2 runtime is not installed on this runner' }
Write-Host "WebView2 runtime installed: $version"

$dest = Join-Path $env:RUNNER_TEMP 'msedgedriver'
New-Item -ItemType Directory -Force $dest | Out-Null
$driver = Join-Path $dest 'msedgedriver.exe'
$zip = Join-Path $dest 'edgedriver.zip'

Write-Host "Downloading msedgedriver $version ..."
Invoke-WebRequest -Uri "https://msedgedriver.microsoft.com/$version/edgedriver_win64.zip" -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath $dest -Force
Remove-Item -LiteralPath $zip -Force

& (Join-Path $PSScriptRoot 'assert-microsoft-signature.ps1') -Path $driver
$have = (& $driver --version) -replace '.*WebDriver\s+([\d.]+).*', '$1'
if ($have -ne $version) { throw "downloaded msedgedriver reports version $have, expected $version" }
Write-Host "  verified: $driver ($have)"

"EIR_E2E_MSEDGEDRIVER=$driver" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
