#Requires -Version 5.1
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$version = '150.0.4078.105'
$cabName = "Microsoft.WebView2.FixedVersionRuntime.$version.x64.cab"
$runtimeName = "Microsoft.WebView2.FixedVersionRuntime.$version.x64"
$url = 'https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/b401c036-cfb8-4dc4-a58e-8766441df4ac/Microsoft.WebView2.FixedVersionRuntime.150.0.4078.105.x64.cab'
$expectedHash = '26C07CAD95615A672CDE8C1843A326E18AD25D691F004347544E5E099BFF9B92'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$downloadDir = Join-Path $repoRoot 'target\webview2'
$cabPath = Join-Path $downloadDir $cabName
$runtimePath = Join-Path $repoRoot "eir-ui\$runtimeName"

New-Item -ItemType Directory -Force -Path $downloadDir | Out-Null
if (-not (Test-Path -LiteralPath $cabPath -PathType Leaf)) {
    $partial = "$cabPath.partial"
    if (Test-Path -LiteralPath $partial) {
        Remove-Item -LiteralPath $partial -Force
    }
    Start-BitsTransfer -Source $url -Destination $partial
    Move-Item -LiteralPath $partial -Destination $cabPath
}

$stream = [System.IO.File]::OpenRead($cabPath)
$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
    $actualHash = [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '')
}
finally {
    $sha256.Dispose()
    $stream.Dispose()
}
if ($actualHash -ne $expectedHash) {
    throw "WebView2 archive hash mismatch: expected $expectedHash, got $actualHash"
}

$securityModule = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1'
if ($PSVersionTable.PSEdition -eq 'Desktop') {
    Import-Module $securityModule -Force
} elseif (-not (Get-Command Get-AuthenticodeSignature -ErrorAction SilentlyContinue)) {
    throw 'Get-AuthenticodeSignature is unavailable'
}

$extractRoot = Join-Path $repoRoot "eir-ui\.webview2-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $extractRoot | Out-Null
try {
    & "$env:SystemRoot\System32\expand.exe" $cabPath '-F:*' $extractRoot | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "expand.exe failed with exit code $LASTEXITCODE"
    }
    $extractedRuntime = Join-Path $extractRoot $runtimeName
    $extractedRuntimeExe = Join-Path $extractedRuntime 'msedgewebview2.exe'
    if (-not (Test-Path -LiteralPath $extractedRuntimeExe -PathType Leaf)) {
        throw 'Expanded WebView2 runtime is incomplete'
    }
    if ((Get-AuthenticodeSignature -LiteralPath $extractedRuntimeExe).Status -ne 'Valid') {
        throw 'WebView2 runtime signature is invalid'
    }

    if (Test-Path -LiteralPath $runtimePath) {
        try {
            [IO.File]::Delete($runtimePath)
        }
        catch [UnauthorizedAccessException] {
            [IO.Directory]::Delete($runtimePath, $true)
        }
    }
    Move-Item -LiteralPath $extractedRuntime -Destination $runtimePath
}
finally {
    if (Test-Path -LiteralPath $extractRoot) {
        Remove-Item -LiteralPath $extractRoot -Recurse -Force
    }
}

[pscustomobject]@{
    CabPath = $cabPath
    RuntimePath = $runtimePath
    Version = $version
}
