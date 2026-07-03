#Requires -Version 5.1
<#
.SYNOPSIS
    Verify the release version is identical across all four manifests.
.DESCRIPTION
    Eir's version lives in four files that must move together (see ARCHITECTURE.md
    "Version-bump locations"): the three crate Cargo.toml files and
    eir-ui/tauri.conf.json. Nothing else enforces this, so a partial bump would ship a
    self-update whose installer name / About box / updater compare disagree with the
    binaries. This script fails (exit 1) on any mismatch so CI catches drift before a
    tag is cut. Returns a structured object describing each source for the caller.
#>
[CmdletBinding()]
param(
    [string] $RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

function Get-CargoVersion {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Path)
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ($line -match '^\s*\[package\]') { $inPackage = $true; continue }
        if ($line -match '^\s*\[') { $inPackage = $false; continue }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "No [package] version found in $Path"
}

function Get-TauriVersion {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Path)
    $json = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    if (-not $json.version) { throw "No version field in $Path" }
    return $json.version
}

$sources = @(
    [pscustomobject]@{ File = 'eir-proto/Cargo.toml';    Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-proto/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-svc/Cargo.toml';      Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-svc/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-ui/Cargo.toml';       Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-ui/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-ui/tauri.conf.json';  Version = (Get-TauriVersion (Join-Path $RepoRoot 'eir-ui/tauri.conf.json')) }
)

$sources | Format-Table -AutoSize | Out-String | Write-Host

# Wrap in @() so a single unique value stays an array (a bare string would index by
# character, not element).
$distinct = @($sources.Version | Sort-Object -Unique)
if ($distinct.Count -ne 1) {
    Write-Error "Version mismatch across manifests: $($distinct -join ', '). All four must match."
    exit 1
}

Write-Host "All four manifests agree on version $($distinct[0])." -ForegroundColor Green
exit 0
