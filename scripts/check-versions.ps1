#Requires -Version 5.1
<#
.SYNOPSIS
    Verify the release version is identical across manifests and Cargo.lock.
.DESCRIPTION
    Eir's version lives in four manifests whose three package entries must also
    match Cargo.lock (see ARCHITECTURE.md
    "Version-bump locations"): the three crate Cargo.toml files and
    eir-ui/tauri.conf.json. A partial bump would ship a
    self-update whose installer name / About box / updater compare disagree with the
    binaries. This script fails (exit 1) on any mismatch so CI catches drift before a
    tag is cut.
#>
[CmdletBinding()]
param(
    [string] $RepoRoot,
    [string] $ExpectedTag
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Resolve the repo root in the body, not a param default: $PSScriptRoot is not reliably
# populated during param-default binding under `powershell -File` in CI, which made
# `Split-Path -Parent ''` throw. Prefer the script's own dir; fall back to the current
# directory (CI runs this from the repo root).
if (-not $RepoRoot) {
    if ($PSScriptRoot) {
        $RepoRoot = Split-Path -Parent $PSScriptRoot
    } else {
        $RepoRoot = (Get-Location).Path
    }
}

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

function Get-CargoLockVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Package
    )
    $versions = @()
    $name = $null
    $version = $null
    foreach ($line in @((Get-Content -LiteralPath $Path) + '[[package]]')) {
        if ($line -match '^\s*\[\[package\]\]') {
            if ($name -eq $Package -and $version) {
                $versions += $version
            }
            $name = $null
            $version = $null
        } elseif ($line -match '^\s*name\s*=\s*"([^"]+)"') {
            $name = $Matches[1]
        } elseif ($line -match '^\s*version\s*=\s*"([^"]+)"') {
            $version = $Matches[1]
        }
    }
    if ($versions.Count -ne 1) {
        throw "Expected one Cargo.lock entry for $Package, found $($versions.Count)."
    }
    return $versions[0]
}

$lockPath = Join-Path $RepoRoot 'Cargo.lock'
$sources = @(
    [pscustomobject]@{ File = 'eir-proto/Cargo.toml';    Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-proto/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-svc/Cargo.toml';      Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-svc/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-ui/Cargo.toml';       Version = (Get-CargoVersion (Join-Path $RepoRoot 'eir-ui/Cargo.toml')) }
    [pscustomobject]@{ File = 'eir-ui/tauri.conf.json';  Version = (Get-TauriVersion (Join-Path $RepoRoot 'eir-ui/tauri.conf.json')) }
    [pscustomobject]@{ File = 'Cargo.lock (eir-proto)';  Version = (Get-CargoLockVersion $lockPath 'eir-proto') }
    [pscustomobject]@{ File = 'Cargo.lock (eir-svc)';    Version = (Get-CargoLockVersion $lockPath 'eir-svc') }
    [pscustomobject]@{ File = 'Cargo.lock (eir-ui)';     Version = (Get-CargoLockVersion $lockPath 'eir-ui') }
)

$sources | Format-Table -AutoSize | Out-String | Write-Host

# Wrap in @() so a single unique value stays an array (a bare string would index by
# character, not element).
$distinct = @($sources.Version | Sort-Object -Unique)
if ($distinct.Count -ne 1) {
    Write-Error "Version mismatch across manifests and Cargo.lock: $($distinct -join ', ')."
    exit 1
}

$version = $distinct[0]
if ($ExpectedTag -and (-not [string]::Equals($ExpectedTag, "v$version", [StringComparison]::Ordinal))) {
    Write-Error "Release tag '$ExpectedTag' must exactly equal 'v$version'."
    exit 1
}

Write-Host "All manifests and Cargo.lock agree on version $version." -ForegroundColor Green
exit 0
