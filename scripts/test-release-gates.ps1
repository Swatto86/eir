#Requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

function Read-RepoFile {
    param([Parameter(Mandatory)][string] $Path)
    Get-Content -LiteralPath (Join-Path $repoRoot $Path) -Raw
}

function Assert-Contains {
    param(
        [Parameter(Mandatory)][string] $Text,
        [Parameter(Mandatory)][string] $Pattern,
        [Parameter(Mandatory)][string] $Failure
    )
    if ($Text -notmatch $Pattern) {
        throw $Failure
    }
}

$ci = Read-RepoFile '.github\workflows\ci.yml'
$release = Read-RepoFile '.github\workflows\release.yml'
$publish = Read-RepoFile 'scripts\publish-release.ps1'
$versions = Read-RepoFile 'scripts\check-versions.ps1'
$prepare = Read-RepoFile 'scripts\prepare-webview2.ps1'
$buildService = Read-RepoFile 'eir-ui\build-svc.ps1'
$serviceSmoke = Read-RepoFile 'scripts\service-smoke.ps1'
$portableSmoke = Read-RepoFile 'scripts\portable-smoke.ps1'
$verify = Read-RepoFile 'scripts\verify.ps1'
$fastcheck = Read-RepoFile 'scripts\fastcheck.ps1'

Assert-Contains $versions 'Cargo\.lock' 'Version gate does not inspect Cargo.lock.'
foreach ($package in 'eir-proto', 'eir-svc', 'eir-ui') {
    $lockCall = 'Get-CargoLockVersion\s+\$lockPath\s+''' + [regex]::Escape($package) + ''''
    Assert-Contains $versions $lockCall "Version gate does not check Cargo.lock package '$package'."
}
Assert-Contains $versions 'ExpectedTag' 'Version gate cannot require the exact manifest tag.'
Assert-Contains $versions '\[string\]::Equals\(\$ExpectedTag,\s*"v\$version",\s*\[StringComparison\]::Ordinal\)' 'Version gate does not require the exact case-sensitive v<manifest version> tag.'
Assert-Contains $release 'check-versions\.ps1[^\r\n]*-ExpectedTag' 'Release does not require tag v<manifest version>.'

Assert-Contains $release 'GITHUB_SHA' 'Release gate does not bind verification to the checked-out tag SHA.'
Assert-Contains $release 'cargo fmt --all --check' 'Release does not rerun formatting on the tag SHA.'
Assert-Contains $release 'cargo clippy --locked --workspace --all-targets -- -D warnings' 'Release does not rerun locked clippy on the tag SHA.'
Assert-Contains $release 'cargo test --locked --workspace --all-targets' 'Release does not rerun locked tests on the tag SHA.'
Assert-Contains $release 'cargo-deny-action@v2' 'Release does not rerun the dependency audit on the tag SHA.'
Assert-Contains $release '(?m)^\s+needs:\s+dependency-audit\s*$' 'Windows release can publish without the tag-SHA dependency audit.'
Assert-Contains $release 'service-smoke\.ps1' 'Release does not smoke the installed LocalSystem service before publishing.'
Assert-Contains $serviceSmoke '(?s)& \$smokeExe uninstall\s+if \(\$LASTEXITCODE -ne 0\).*?Get-Service -Name \$serviceName -ErrorAction SilentlyContinue.*?throw "Service registration remained after uninstall' 'Installed-service smoke does not fail when uninstall fails or leaves its registration behind.'
Assert-Contains $release 'portable-smoke\.ps1' 'Release does not smoke the self-contained portable before publishing.'
Assert-Contains $release 'check-portable-imports\.ps1' 'Release does not reject non-self-contained binaries before publishing.'
Assert-Contains $portableSmoke 'Test-ProcessPathInDirectory' 'Portable smoke does not scope process cleanup to its exact extraction directory.'
Assert-Contains $portableSmoke '(?s)if \(\(-not \$portableService\) -and \$extractDir\).*?Test-ProcessPathInDirectory' 'Portable smoke cleanup can select a service without a known extraction directory.'
if ($portableSmoke -match '\[System\.IO\.Path\]::GetTempPath\(\)') {
    throw 'Portable smoke may kill an unrelated service process from the shared temp directory.'
}
Assert-Contains $release 'test-portable-runner\.ps1' 'Release does not exercise portable lifecycle invariants before publishing.'
Assert-Contains $ci 'test-portable-runner\.ps1' 'CI does not exercise portable lifecycle invariants.'
Assert-Contains $verify 'test-portable-runner\.ps1' 'Local verify does not exercise portable lifecycle invariants.'

foreach ($command in @(
    './scripts/test-installer-hooks.ps1',
    './scripts/test-release-gates.ps1',
    './scripts/test-portable-runner.ps1',
    'node --check ui/main.js',
    'cargo fmt --all --check'
)) {
    $guardedCommand = '(?m)^[ ]{10}' + [regex]::Escape($command) +
        '\r?\n[ ]{10}if \(\$LASTEXITCODE -ne 0\) \{ exit \$LASTEXITCODE \}'
    Assert-Contains $release $guardedCommand "Release can swallow a failed packaging command: $command"
}

foreach ($pair in @(
    @($buildService, 'cargo build --locked -p eir-svc --release', 'Service build is not locked.'),
    @($buildService, 'cargo metadata --locked --no-deps', 'Service staging metadata is not locked.'),
    @($verify, 'cargo clippy --locked --workspace', 'Local clippy gate is not locked.'),
    @($verify, 'cargo test --locked --workspace', 'Local test gate is not locked.'),
    @($verify, 'cargo build --locked --workspace --release', 'Local release build is not locked.'),
    @($fastcheck, 'cargo clippy --locked --workspace', 'Fast clippy gate is not locked.'),
    @($ci, 'cargo clippy --locked --workspace', 'CI clippy gate is not locked.'),
    @($ci, 'cargo test --locked --workspace', 'CI test gate is not locked.'),
    @($ci, 'args:\s*-- --locked', 'CI Tauri build is not locked.'),
    @($release, 'args:\s*-- --locked', 'Release Tauri build is not locked.')
)) {
    Assert-Contains -Text $pair[0] -Pattern $pair[1] -Failure $pair[2]
}

if ($ci -match 'eir-ui/Microsoft\.WebView2\.FixedVersionRuntime' -or
    $release -match 'eir-ui/Microsoft\.WebView2\.FixedVersionRuntime') {
    throw 'Expanded WebView2 runtime must not be restored from the workflow cache.'
}
Assert-Contains $prepare '(?s)Get-AuthenticodeSignature.*Move-Item[^\r\n]*\$extractedRuntime[^\r\n]*\$runtimePath' 'WebView2 extraction is not verified before replacing the runtime.'
Assert-Contains $prepare '\$env:SystemRoot[^\r\n]*Microsoft\.PowerShell\.Security\.psd1' 'WebView2 preparation does not use the trusted absolute signature-module path.'
Assert-Contains $prepare '(?s)PSEdition -eq ''Desktop''.*?Import-Module \$securityModule -Force.*?Get-AuthenticodeSignature' 'Windows PowerShell does not explicitly load the trusted signature module before verification.'
if ($prepare -match '(?s)if\s*\(-not\s*\(Test-Path[^\r\n]*\$runtimeExe.*?expand\.exe') {
    throw 'WebView2 runtime can bypass verified CAB extraction through local reuse.'
}

if ($release -match '(?m)^\s+tagName:') {
    throw 'Release Tauri build must not upload to GitHub; draft asset replacement is retried in publish-release.ps1.'
}
Assert-Contains $release 'scripts/publish-release\.ps1' 'Release does not invoke the retried publish script.'
Assert-Contains $publish '\$installerName\s*=\s*"Eir_\$\{version\}_x64-setup\.exe"' 'Release does not require the exact versioned installer filename.'
Assert-Contains $publish 'Get-Item -LiteralPath \$installerPath' 'Release does not select the exact installer path.'
Assert-Contains $publish '\$installerSignatureName\s*=\s*"\$\(\$installer\.Name\)\.sig"' 'Release does not require the exact installer signature asset.'
Assert-Contains $publish '\$latest\.version\s+-ne\s+\$version' 'Release does not validate latest.json version.'
Assert-Contains $publish 'releases/download/\$tag/\$\(\$installer\.Name\)' 'Release does not require latest.json to target the exact tagged installer.'
Assert-Contains $publish '\[string\]::Equals\(\$metadataSignature,\s*\$installerSignature' 'Release does not match latest.json signature to the exact installer signature asset.'
Assert-Contains $publish 'repos/\$repo/releases\?per_page=100' 'Release does not list drafts by id; get-by-tag 404s unpublished releases.'
Assert-Contains $publish "'--method', 'PATCH'" 'Release does not publish the verified draft through the Releases API.'
Assert-Contains $publish "'-F', 'draft=false'" 'Release does not publish only after verification.'
Assert-Contains $publish 'upload_url' 'Publish does not use the draft release upload_url for assets.'
Assert-Contains $publish 'uploads\.github\.com' 'Publish does not require the GitHub uploads host for release assets.'
Assert-Contains $publish 'for \(\$attempt = 1; \$attempt -le \$maxAttempts' 'Publish does not retry transient GitHub API failures.'
Assert-Contains $publish 'Start-Sleep -Seconds \$delaySeconds' 'Publish retry does not back off between GitHub API attempts.'
if ($publish -match '(?m)\s--output(\s|$)') {
    throw 'gh api has no --output flag; download release assets from redirected stdout.'
}
Assert-Contains $publish 'StandardOutput' 'Publish does not stream gh api stdout when downloading assets for validation.'
Assert-Contains $publish 'unknown flag' 'Publish retries gh usage errors instead of failing closed.'

[void][scriptblock]::Create($publish)

Write-Host 'release-gate checks OK' -ForegroundColor Green
