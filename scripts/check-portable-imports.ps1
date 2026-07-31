#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$EirExe,
    [Parameter(Mandatory)][string]$ServiceExe
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

foreach ($path in @($EirExe, $ServiceExe)) {
    $resolved = (Resolve-Path -LiteralPath $path).Path
    $image = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($resolved))
    $imports = [regex]::Matches(
        $image,
        '(?i)\b(?:vcruntime\d+(?:_\d+)?|msvcp\d+(?:_\d+)?|webview2loader)\.dll\b'
    ) | ForEach-Object { $_.Value.ToLowerInvariant() } | Sort-Object -Unique
    if ($imports) {
        throw "$(Split-Path -Leaf $resolved) is not self-contained: $($imports -join ', ')"
    }
}

Write-Host 'portable dependency imports OK' -ForegroundColor Green
