#Requires -Version 7.0
[CmdletBinding()]
param()

# Upload signed installer artifacts, the smoke-tested portable, and checksums to
# the draft GitHub release for $env:GITHUB_REF_NAME, verify updater metadata,
# then publish. GitHub API calls retry on transient failures (the tag workflow
# previously died on 503s while replacing draft assets inside tauri-action).

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$tag = $env:GITHUB_REF_NAME
$sha = $env:GITHUB_SHA
$repo = $env:GITHUB_REPOSITORY
$runnerTemp = $env:RUNNER_TEMP
if ([string]::IsNullOrWhiteSpace($tag)) { throw 'GITHUB_REF_NAME is not set' }
if ([string]::IsNullOrWhiteSpace($sha)) { throw 'GITHUB_SHA is not set' }
if ([string]::IsNullOrWhiteSpace($repo)) { throw 'GITHUB_REPOSITORY is not set' }
if ([string]::IsNullOrWhiteSpace($runnerTemp)) { throw 'RUNNER_TEMP is not set' }

$version = $tag -replace '^v', ''
$portable = "target/release/Eir_${tag}_windows-x64.exe"
if (-not (Test-Path -LiteralPath $portable -PathType Leaf)) {
    throw "Smoke-tested portable package is missing: $portable"
}

$installerName = "Eir_${version}_x64-setup.exe"
$installerPath = @(
    Join-Path 'target/release/bundle/nsis' $installerName
    Join-Path 'eir-ui/target/release/bundle/nsis' $installerName
) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if (-not $installerPath) {
    throw "Expected NSIS installer is missing: $installerName"
}
$installer = Get-Item -LiteralPath $installerPath
$installerSignatureName = "$($installer.Name).sig"
$installerSignaturePath = Join-Path $installer.DirectoryName $installerSignatureName
if (-not (Test-Path -LiteralPath $installerSignaturePath -PathType Leaf)) {
    throw "Expected installer signature is missing: $installerSignatureName"
}

$checksums = "target/release/Eir_${tag}_SHA256SUMS.txt"
@($portable, $installer.FullName) | ForEach-Object {
    $hash = (Get-FileHash $_ -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $(Split-Path $_ -Leaf)"
} | Set-Content $checksums -Encoding ascii

$releaseNotes = @'
## Eir {tag}

Autonomous Windows system repair agent. Eir monitors system health and
uses AI to diagnose and fix problems automatically — service failures,
disk pressure, log corruption, driver issues, and more.

### Install
1. Download and run the Windows setup `.exe` below as Administrator.
   The installer registers and starts the `EirSvc` Windows service and seeds `config.toml` automatically.
2. Pick your AI provider and model in **Settings** — use OpenRouter, a logged-in Claude/Codex/Kilo CLI subscription, or an Anthropic API key.
3. Launch **Eir** from the Start Menu — the tray icon appears once the service connects.

Already running an earlier version? It updates itself automatically.

### Portable
`Eir_{tag}_windows-x64.exe` launches directly with no installer.
It uses a session-scoped non-admin foreground service; installing the
LocalSystem background service still requires the setup package.

### Uninstall
Use **Add or Remove Programs** — the uninstaller stops and unregisters the service automatically.
'@.Replace('{tag}', $tag)

$expectedUrl = "https://github.com/$repo/releases/download/$tag/$($installer.Name)"
$installerSignature = (Get-Content -LiteralPath $installerSignaturePath -Raw).Trim()
$latestObject = [ordered]@{
    version   = $version
    notes     = $releaseNotes
    pub_date  = [datetime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ss.fffZ')
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $installerSignature
            url       = $expectedUrl
        }
        'windows-x86_64-nsis' = [ordered]@{
            signature = $installerSignature
            url       = $expectedUrl
        }
    }
}
$latestPath = Join-Path $runnerTemp 'latest.json'
[System.IO.File]::WriteAllText($latestPath, ($latestObject | ConvertTo-Json -Depth 8))

function Invoke-GitHubApi {
    param(
        [Parameter(Mandatory)][string[]]$ApiArguments,
        [string]$InputPath,
        [string]$OutputPath
    )
    $maxAttempts = 6
    $delaySeconds = 4
    for ($attempt = 1; $attempt -le $maxAttempts; $attempt++) {
        $ghArguments = [System.Collections.Generic.List[string]]::new()
        [void]$ghArguments.Add('api')
        foreach ($argument in $ApiArguments) {
            [void]$ghArguments.Add($argument)
        }
        $output = $null
        if ($InputPath) {
            $output = & gh @ghArguments --input $InputPath
        } elseif ($OutputPath) {
            & gh @ghArguments --output $OutputPath
        } else {
            $output = & gh @ghArguments
        }
        if ($LASTEXITCODE -eq 0) {
            return $output
        }
        if ($attempt -eq $maxAttempts) {
            throw "GitHub API failed after $maxAttempts attempts ($($ApiArguments -join ' '))"
        }
        Start-Sleep -Seconds $delaySeconds
        $delaySeconds = [Math]::Min($delaySeconds * 2, 32)
    }
}

# GET /releases/tags/{tag} 404s drafts. List releases so an existing draft is
# visible to GITHUB_TOKEN, then operate by numeric id.
$releasesJson = Invoke-GitHubApi -ApiArguments @('--paginate', "repos/$repo/releases?per_page=100")
$tagReleases = @($releasesJson | ConvertFrom-Json | Where-Object { $_.tag_name -eq $tag })
$published = @($tagReleases | Where-Object { -not $_.draft })
if ($published.Count -gt 0) {
    throw "Release $tag was published before all assets were uploaded"
}

$payloadPath = Join-Path $runnerTemp "eir-$tag-release.json"
$payload = [ordered]@{
    tag_name         = $tag
    target_commitish = $sha
    name             = "Eir $tag"
    body             = $releaseNotes
    draft            = $true
    prerelease       = $false
}
[System.IO.File]::WriteAllText($payloadPath, ($payload | ConvertTo-Json -Depth 5))

$drafts = @($tagReleases | Where-Object { $_.draft })
$release = if ($drafts.Count -gt 0) { $drafts[0] } else { $null }
$jsonHeaders = @('-H', 'Content-Type: application/json')
if ($null -eq $release) {
    $release = (Invoke-GitHubApi -ApiArguments (@('--method', 'POST', "repos/$repo/releases") + $jsonHeaders) -InputPath $payloadPath) | ConvertFrom-Json
} else {
    $release = (Invoke-GitHubApi -ApiArguments (@('--method', 'PATCH', "repos/$repo/releases/$($release.id)") + $jsonHeaders) -InputPath $payloadPath) | ConvertFrom-Json
}
if (-not $release.draft) {
    throw "Release $tag was published before all assets were uploaded"
}
$releaseId = $release.id

function Publish-ReleaseAsset {
    param([long]$ReleaseId, [string]$Path)
    $name = Split-Path $Path -Leaf
    $assetsJson = Invoke-GitHubApi -ApiArguments @("repos/$repo/releases/$ReleaseId/assets?per_page=100")
    foreach ($asset in @($assetsJson | ConvertFrom-Json | Where-Object { $_.name -eq $name })) {
        Invoke-GitHubApi -ApiArguments @('--method', 'DELETE', "repos/$repo/releases/assets/$($asset.id)") | Out-Null
    }
    Invoke-GitHubApi -ApiArguments @(
        '--method', 'POST',
        "repos/$repo/releases/$ReleaseId/assets?name=$([uri]::EscapeDataString($name))",
        '-H', 'Content-Type: application/octet-stream'
    ) -InputPath $Path | Out-Null
}

Publish-ReleaseAsset -ReleaseId $releaseId -Path $installer.FullName
Publish-ReleaseAsset -ReleaseId $releaseId -Path $installerSignaturePath
Publish-ReleaseAsset -ReleaseId $releaseId -Path $latestPath
Publish-ReleaseAsset -ReleaseId $releaseId -Path $portable
Publish-ReleaseAsset -ReleaseId $releaseId -Path $checksums

$releaseJson = Invoke-GitHubApi -ApiArguments @("repos/$repo/releases/$releaseId")
$release = $releaseJson | ConvertFrom-Json
if (-not $release.draft) {
    throw "Release $tag was published before asset verification completed"
}

$assetNames = @($release.assets | ForEach-Object { $_.name })
$requiredAssets = @(
    $installer.Name
    $installerSignatureName
    'latest.json'
    (Split-Path $portable -Leaf)
    (Split-Path $checksums -Leaf)
)
$missingAssets = @($requiredAssets | Where-Object { $assetNames -notcontains $_ })
if ($missingAssets.Count -gt 0) {
    throw "Release assets missing: $($missingAssets -join ', ')"
}

$validationDir = Join-Path $runnerTemp "eir-release-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $validationDir | Out-Null

function Save-ReleaseAsset {
    param([string]$Name)
    $asset = @($release.assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1)
    if (-not $asset) { throw "Failed to download $Name for validation" }
    $out = Join-Path $validationDir $Name
    Invoke-GitHubApi -ApiArguments @(
        '-H', 'Accept: application/octet-stream',
        "repos/$repo/releases/assets/$($asset.id)"
    ) -OutputPath $out | Out-Null
}
Save-ReleaseAsset -Name 'latest.json'
Save-ReleaseAsset -Name $installerSignatureName

$latest = Get-Content (Join-Path $validationDir 'latest.json') -Raw | ConvertFrom-Json
if ($latest.version -ne $version) {
    throw "latest.json version '$($latest.version)' does not match '$version'"
}
$platforms = @($latest.platforms.PSObject.Properties |
    Where-Object { $_.Name -eq 'windows-x86_64' })
if ($platforms.Count -ne 1) {
    throw 'latest.json must contain exactly one windows-x86_64 updater target'
}
$platform = $platforms[0].Value
if (-not [string]::Equals([string]$platform.url, $expectedUrl, [StringComparison]::Ordinal)) {
    throw "latest.json URL '$($platform.url)' does not target '$expectedUrl'"
}
$remoteSignature = (Get-Content (Join-Path $validationDir $installerSignatureName) -Raw).Trim()
$metadataSignature = ([string]$platform.signature).Trim()
if (-not [string]::Equals($metadataSignature, $installerSignature, [StringComparison]::Ordinal)) {
    throw 'latest.json signature does not match the exact installer signature asset'
}
if (-not [string]::Equals($remoteSignature, $installerSignature, [StringComparison]::Ordinal)) {
    throw 'Uploaded installer signature does not match the local signed artifact'
}

Invoke-GitHubApi -ApiArguments @(
    '--method', 'PATCH',
    "repos/$repo/releases/$releaseId",
    '-f', "tag_name=$tag",
    '-F', 'draft=false'
) | Out-Null
