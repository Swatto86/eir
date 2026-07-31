#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$EirExe,
    [Parameter(Mandatory)][string]$ServiceExe,
    [Parameter(Mandatory)][string]$Output,
    [ValidateRange(1, 3600)][int]$IExpressTimeoutSeconds = 600
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$eirPath = (Resolve-Path -LiteralPath $EirExe).Path
$servicePath = (Resolve-Path -LiteralPath $ServiceExe).Path
& (Join-Path $PSScriptRoot 'check-portable-imports.ps1') `
    -EirExe $eirPath -ServiceExe $servicePath
$outputPath = [System.IO.Path]::GetFullPath($Output)
$outputDir = Split-Path -Parent $outputPath
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
$outputBase = [IO.Path]::GetFileNameWithoutExtension($outputPath)
$iexpressTemporaryFiles = @(
    (Join-Path $outputDir "~$outputBase.CAB"),
    (Join-Path $outputDir "~$outputBase.DDF"),
    (Join-Path $outputDir "~${outputBase}_LAYOUT.INF"),
    (Join-Path $outputDir "~$outputBase.RPT")
)
foreach ($path in @($outputPath) + $iexpressTemporaryFiles) {
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Force
    }
}

$runtime = & (Join-Path $PSScriptRoot 'prepare-webview2.ps1')
$cabPath = $runtime.CabPath
$launcherPath = (Resolve-Path (Join-Path $PSScriptRoot 'portable-launch.cmd')).Path
$runnerPath = (Resolve-Path (Join-Path $PSScriptRoot 'portable-run.ps1')).Path
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "eir-portable-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $stage | Out-Null
$iexpressProcess = $null

try {
    Copy-Item -LiteralPath $eirPath -Destination (Join-Path $stage 'eir.exe')
    Copy-Item -LiteralPath $servicePath -Destination (Join-Path $stage 'eir-svc.exe')
    Copy-Item -LiteralPath $cabPath -Destination $stage
    Copy-Item -LiteralPath $launcherPath -Destination $stage
    Copy-Item -LiteralPath $runnerPath -Destination $stage
    Copy-Item -LiteralPath (Join-Path $repoRoot 'config.toml.example') `
        -Destination (Join-Path $stage 'config.toml')
    Copy-Item -LiteralPath (Join-Path $repoRoot 'policy.toml') -Destination $stage

    $cabName = Split-Path -Leaf $cabPath
    $sedPath = Join-Path $stage 'portable.sed'
    @"
[Version]
Class=IEXPRESS
SEDVersion=3
[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=0
HideExtractAnimation=1
UseLongFileName=1
InsideCompressed=0
Compress=0
CAB_FixedSize=0
CAB_ResvCodeSigning=0
RebootMode=N
InstallPrompt=
DisplayLicense=
FinishMessage=
TargetName=$outputPath
FriendlyName=Eir portable
AppLaunched=cmd.exe /d /c portable-launch.cmd
PostInstallCmd=<None>
AdminQuietInstCmd=
UserQuietInstCmd=
SourceFiles=SourceFiles
[SourceFiles]
SourceFiles0=$stage\
[SourceFiles0]
%FILE0%=
%FILE1%=
%FILE2%=
%FILE3%=
%FILE4%=
%FILE5%=
%FILE6%=
[Strings]
FILE0="eir.exe"
FILE1="eir-svc.exe"
FILE2="$cabName"
FILE3="portable-launch.cmd"
FILE4="portable-run.ps1"
FILE5="config.toml"
FILE6="policy.toml"
"@ | Set-Content -LiteralPath $sedPath -Encoding Ascii

    $iexpressProcess = Start-Process -FilePath "$env:SystemRoot\System32\iexpress.exe" `
        -ArgumentList @('/N', '/Q', $sedPath) -PassThru -WindowStyle Hidden
    if (-not $iexpressProcess.WaitForExit($IExpressTimeoutSeconds * 1000)) {
        throw "IExpress did not finish within $IExpressTimeoutSeconds seconds"
    }
    if ($iexpressProcess.ExitCode -ne 0) {
        throw "IExpress failed with exit code $($iexpressProcess.ExitCode)"
    }
    if (-not (Test-Path -LiteralPath $outputPath -PathType Leaf)) {
        throw "IExpress did not create $outputPath"
    }
}
finally {
    if ($iexpressProcess -and (-not $iexpressProcess.HasExited)) {
        & "$env:SystemRoot\System32\taskkill.exe" /PID $iexpressProcess.Id /T /F | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Could not terminate timed-out IExpress process tree $($iexpressProcess.Id)"
        }
    }
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
    foreach ($path in $iexpressTemporaryFiles) {
        if (Test-Path -LiteralPath $path) {
            Remove-Item -LiteralPath $path -Force
        }
    }
}

Get-Item -LiteralPath $outputPath
