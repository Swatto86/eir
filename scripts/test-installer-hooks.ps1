#Requires -Version 5.1
[CmdletBinding()]
param(
    [switch] $RequireCompiler
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$hooksPath = Join-Path $repoRoot 'eir-ui\installer-hooks.nsh'
$configPath = Join-Path $repoRoot 'eir-ui\tauri.conf.json'
$serviceInstallPath = Join-Path $repoRoot 'eir-svc\src\service_install.rs'
$serviceMainPath = Join-Path $repoRoot 'eir-svc\src\main.rs'
$hooks = Get-Content -LiteralPath $hooksPath -Raw
$config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
$serviceInstall = Get-Content -LiteralPath $serviceInstallPath -Raw
$serviceMain = Get-Content -LiteralPath $serviceMainPath -Raw

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

$unsafeAcl = Select-String -LiteralPath $hooksPath -Pattern 'icacls\.exe.*(?:^|\s)/T(?:\s|$)'
if ($unsafeAcl) {
    throw "Installer ACL reset must not recurse through an existing tree: line $($unsafeAcl.LineNumber)."
}
if ($hooks -match '(?im)^\s*RMDir\s+/r\s+') {
    throw 'Installer hooks must not recursively delete attacker-controlled paths.'
}

Assert-Contains $hooks 'GetFileAttributesW' 'Missing native INSTDIR attribute check.'
Assert-Contains $hooks '0x400' 'Missing INSTDIR reparse-point rejection.'
Assert-Contains $hooks 'hardlink\s+list' 'Missing state-file hardlink validation.'
Assert-Contains $hooks 'Count\s+-ne\s+1' 'State files must have exactly one hardlink.'
$migrationStart = $hooks.IndexOf('Function EirMigrateStateFile', [StringComparison]::Ordinal)
$migrationEnd = $hooks.IndexOf('FunctionEnd', $migrationStart, [StringComparison]::Ordinal)
if ($migrationStart -lt 0 -or $migrationEnd -lt $migrationStart) {
    throw 'State migration function is missing.'
}
$migration = $hooks.Substring($migrationStart, $migrationEnd - $migrationStart)
if ($migration -match 'CopyFiles') {
    throw 'State migration must not reopen a validated user-writable source with NSIS CopyFiles.'
}
Assert-Contains $migration '\[IO\.FileShare\]::Read' 'State migration does not hold a no-write/delete-share source handle.'
Assert-Contains $migration '\[IO\.FileMode\]::CreateNew' 'State migration does not create a fresh protected destination.'
Assert-Contains $migration '\.CopyTo\(' 'State migration does not copy from the held source stream.'
$inPlaceStart = $migration.IndexOf('state_file_copy_in_place:', [StringComparison]::Ordinal)
$legacyStart = $migration.IndexOf('state_file_copy_legacy:', [StringComparison]::Ordinal)
if ($inPlaceStart -lt 0 -or $legacyStart -le $inPlaceStart) {
    throw 'Protected in-place state migration is not separated from legacy migration.'
}
$inPlaceMigration = $migration.Substring($inPlaceStart, $legacyStart - $inPlaceStart)
if ($inPlaceMigration -match 'WindowsPowerShell\\v1\.0\\powershell\.exe') {
    throw 'Protected in-place state migration must not depend on child PowerShell file access.'
}
Assert-Contains $inPlaceMigration '(?s)CreateFileW\(w "\$2", i 0x80000000, i 1,.*?CopyFileW\(w "\$2", w "\$3", i 1\).*?StrCmp \$4 "0" state_file_copy_in_place_failed.*?CloseHandle\(p r5\)' 'Protected in-place migration does not hold the source against writes/deletes through the native copy.'
Assert-Contains $inPlaceMigration 'FlushFileBuffers\(p r5\)' 'Protected in-place migration does not flush the fresh destination.'
$cloneCommand = [regex]::Match(
    $migration,
    '(?m)^\s*nsExec::Exec `".*?-Command "(?<Script>.*)"`\s*$'
)
if (-not $cloneCommand.Success) {
    throw 'State migration PowerShell command could not be inspected.'
}
[void][scriptblock]::Create($cloneCommand.Groups['Script'].Value.Replace('$$', '$'))
if ($hooks -match '\$PLUGINSDIR\\eir-svc\.rollback\.exe') {
    throw 'Service rollback binary must not be staged in user-writable plugin storage.'
}
Assert-Contains $hooks '\$INSTDIR\\\.eir-svc\.rollback\.exe' 'Service rollback binary is not kept under the hardened install root.'
Assert-Contains $hooks '(?s)CopyFiles /SILENT "\$INSTDIR\\eir-svc\.exe" "\$INSTDIR\\\.eir-svc\.rollback\.exe".*?Call EirSecurePath.*?Call EirRequireSafeStateFile.*?StrCpy \$EirServiceRollbackPath "\$INSTDIR\\\.eir-svc\.rollback\.exe"' 'Rollback backup is not secured and validated before it becomes restorable.'
Assert-Contains $hooks '(?s)Function EirRestartServiceAfterFailedInstall.*?Push "\$EirServiceRollbackPath"\s+Call EirRequireSafeStateFile.*?CopyFiles /SILENT "\$EirServiceRollbackPath" "\$INSTDIR\\eir-svc\.exe"' 'Rollback binary is not revalidated immediately before restore.'
Assert-Contains $hooks '(?s)!macro NSIS_HOOK_PREUNINSTALL.*?StrCmp \$UpdateMode "1" preserve_uninstall_data.*?StrCmp \$DeleteAppDataCheckboxState "1" 0 cleanup_user_data_done' 'Uninstaller does not preserve user data during updates or when cleanup is unchecked.'
Assert-Contains $hooks '\$LOCALAPPDATA\\\$\{BUNDLEID\}' 'Uninstaller does not clean the bounded local app-data directory.'
Assert-Contains $hooks 'DeleteRegKey SHCTX "\$\{MANUPRODUCTKEY\}"' 'Checked user-data cleanup does not remove the install-location registry state.'
Assert-Contains $hooks 'StrCpy \$DeleteAppDataCheckboxState 0' 'Uninstaller does not suppress Tauri''s later recursive app-data deletion.'
if ($serviceInstall -match '(?s)\.args\(\[[^\]]*"/T"') {
    throw 'Service installation must not recursively ACL-reset an existing tree.'
}
Assert-Contains $serviceInstall 'nNumberOfLinks\s*==\s*1' 'Service binary hardlinks are not rejected.'
Assert-Contains $serviceMain 'service_install::validate_current_binary\(\)' 'SCM startup does not validate its current service binary.'

$bundleTargets = @(
    "$($config.productName.ToLowerInvariant()).exe"
    'uninstall.exe'
)
$bundleTargets += @($config.bundle.resources.PSObject.Properties.Value)
foreach ($target in $bundleTargets | Sort-Object -Unique) {
    $escaped = [regex]::Escape("!insertmacro EirRemoveBundleOutput `"$target`"")
    Assert-Contains $hooks $escaped "Bundle output '$target' is not neutralized before extraction."
}

$fixedRuntime = $config.bundle.windows.webviewInstallMode
if ($fixedRuntime.type -eq 'fixedRuntime') {
    $runtimeName = Split-Path -Leaf $fixedRuntime.path
    $escaped = [regex]::Escape("!insertmacro EirRemoveBundleTree `"$runtimeName`"")
    Assert-Contains $hooks $escaped "Fixed WebView2 runtime '$runtimeName' is not safely replaced before extraction."
}

$stateFiles = @(
    'config.toml'
    'config.toml.bak'
    'eir.db'
    'eir.db-wal'
    'eir.db-shm'
    'eir.log'
)
foreach ($state in $stateFiles) {
    $migrate = [regex]::Escape("!insertmacro EirMigrateState `"$state`"")
    $cleanup = [regex]::Escape("!insertmacro EirCleanupLegacyState `"$state`"")
    Assert-Contains $hooks $migrate "State file '$state' is not migrated through the safe clone path."
    Assert-Contains $hooks $cleanup "Legacy state file '$state' is not deleted after success."
}

$serviceOk = $hooks.IndexOf('service_install_ok:', [StringComparison]::Ordinal)
$legacyCleanup = $hooks.IndexOf('!insertmacro EirCleanupLegacyState', [StringComparison]::Ordinal)
if ($serviceOk -lt 0 -or $legacyCleanup -lt $serviceOk) {
    throw 'Legacy state cleanup must occur only after the service install succeeds.'
}

$compilerCandidates = @(
    (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe')
    (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\Bin\makensis.exe')
    (Join-Path ${env:ProgramFiles(x86)} 'NSIS\Bin\makensis.exe')
) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }
$compiler = $compilerCandidates | Select-Object -First 1
if (-not $compiler) {
    if ($RequireCompiler) {
        throw 'makensis.exe is required but was not found.'
    }
    Write-Warning 'makensis.exe not found; static installer-hook checks passed, compile check skipped.'
    return
}

$tempRoot = Join-Path ([IO.Path]::GetTempPath()) ('eir-nsis-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempRoot | Out-Null
try {
    $escapedHooks = $hooksPath.Replace('$', '$$')
    $outFile = (Join-Path $tempRoot 'harness.exe').Replace('$', '$$')
    $harness = @"
Unicode true
Name "Eir installer hook harness"
OutFile "$outFile"
RequestExecutionLevel admin
!include MUI2.nsh
!define PRODUCTNAME "Eir"
!define MAINBINARYNAME "eir"
!define MANUPRODUCTKEY "Software\Eir"
!define MANUKEY "Software"
!define BUNDLEID "co.swatto.eir"
!define ARCH "x64"
Var PassiveMode
Var UpdateMode
Var DeleteAppDataCheckboxState
!macro CheckIfAppIsRunning EXE PRODUCT
!macroend
!include "$escapedHooks"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
Section
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro NSIS_HOOK_POSTINSTALL
SectionEnd
Section Uninstall
  !insertmacro NSIS_HOOK_PREUNINSTALL
  !insertmacro NSIS_HOOK_POSTUNINSTALL
SectionEnd
"@
    $harnessPath = Join-Path $tempRoot 'harness.nsi'
    Set-Content -LiteralPath $harnessPath -Value $harness -Encoding utf8
    & $compiler /V2 $harnessPath
    if ($LASTEXITCODE -ne 0) {
        throw "NSIS installer-hook harness failed with exit code $LASTEXITCODE."
    }
}
finally {
    if (Test-Path -LiteralPath $tempRoot) {
        [IO.Directory]::Delete($tempRoot, $true)
    }
}

Write-Host 'installer-hook checks OK' -ForegroundColor Green
