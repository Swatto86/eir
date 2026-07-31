#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$EirExe,
    [Parameter(Mandatory)][string]$ServiceExe,
    [Parameter(Mandatory)][string]$Output
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Read-FileSuffix {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][long]$Offset
    )

    $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        $share
    )
    try {
        [void]$stream.Seek([Math]::Min($Offset, $stream.Length), [IO.SeekOrigin]::Begin)
        $reader = [IO.StreamReader]::new($stream)
        try {
            $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}

function Test-ProcessPathInDirectory {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Directory
    )

    $prefix = [IO.Path]::GetFullPath($Directory)
    $separator = [IO.Path]::DirectorySeparatorChar.ToString()
    if (-not $prefix.EndsWith($separator, [StringComparison]::Ordinal)) {
        $prefix += $separator
    }
    [IO.Path]::GetFullPath($Path).StartsWith(
        $prefix,
        [StringComparison]::OrdinalIgnoreCase
    )
}

& (Join-Path $PSScriptRoot 'build-portable.ps1') `
    -EirExe $EirExe -ServiceExe $ServiceExe -Output $Output | Out-Null
$package = (Resolve-Path -LiteralPath $Output).Path
$localAppData = [Environment]::GetFolderPath(
    [Environment+SpecialFolder]::LocalApplicationData
)
$stateRoot = Join-Path $localAppData 'EirPortable'
$log = Join-Path $stateRoot 'eir.log'
$logLengthBefore = if (Test-Path -LiteralPath $log -PathType Leaf) {
    (Get-Item -LiteralPath $log -Force).Length
}
else {
    0
}
$started = Get-Date
$outer = $null
$eir = $null
$portableService = $null
$extractDir = $null
$ambientRustLog = [Environment]::GetEnvironmentVariable('RUST_LOG', 'Process')

try {
    $env:RUST_LOG = 'info'
    $outer = Start-Process -FilePath $package -PassThru -WindowStyle Hidden
    for ($i = 0; $i -lt 120 -and -not $eir; $i++) {
        Start-Sleep -Milliseconds 500
        $outer.Refresh()
        if ($outer.HasExited) {
            throw "Portable package exited before Eir started (exit $($outer.ExitCode))"
        }
        $eir = Get-Process eir -ErrorAction SilentlyContinue |
            Where-Object { $_.StartTime -ge $started } |
            Select-Object -First 1
    }
    if (-not $eir) {
        throw 'Portable package did not launch Eir within 60 seconds'
    }

    $extractDir = Split-Path -Parent $eir.Path
    $portableService = Get-Process eir-svc -ErrorAction SilentlyContinue |
        Where-Object {
            $_.StartTime -ge $started -and
            (Test-ProcessPathInDirectory -Path $_.Path -Directory $extractDir)
        } |
        Select-Object -First 1
    if (-not $portableService) {
        throw 'Portable package did not launch its foreground service'
    }
    $fixedRuntime = $null
    for ($i = 0; $i -lt 60 -and -not $fixedRuntime; $i++) {
        Start-Sleep -Milliseconds 500
        $fixedRuntime = Get-Process msedgewebview2 -ErrorAction SilentlyContinue |
            Where-Object {
                $_.StartTime -ge $started -and
                (Test-ProcessPathInDirectory -Path $_.Path -Directory $extractDir) -and
                $_.Path -like '*Microsoft.WebView2.FixedVersionRuntime.*.x64*'
            } |
            Select-Object -First 1
    }
    if (-not $fixedRuntime) {
        throw 'Portable Eir did not use its bundled fixed WebView2 runtime'
    }
    $connected = $false
    for ($i = 0; $i -lt 60 -and (-not $connected); $i++) {
        Start-Sleep -Milliseconds 500
        if (Test-Path -LiteralPath $log -PathType Leaf) {
            $connected = (Read-FileSuffix -Path $log -Offset $logLengthBefore) `
                -like '*UI connected to service pipe*'
        }
    }
    if (-not $connected) {
        throw 'Portable UI did not authenticate and connect to its foreground service'
    }
    foreach ($name in @('config.toml', 'policy.toml', 'eir.log')) {
        if (-not (Test-Path -LiteralPath (Join-Path $stateRoot $name) -PathType Leaf)) {
            throw "Portable state was not persisted: $name"
        }
    }

    Stop-Process -Id $eir.Id
    $eir = $null
    $portableService.Refresh()
    if ((-not $portableService.HasExited) -and (-not $portableService.WaitForExit(45000))) {
        throw 'Portable foreground service survived its runner lease'
    }
    $outer.Refresh()
    if (-not $outer.HasExited) {
        Wait-Process -Id $outer.Id -Timeout 30
    }
    # IExpress removes its extraction tree asynchronously after the launched
    # runner exits; the fixed WebView process tree can take longer than 10s to
    # release its final handles on a busy Windows host.
    for ($i = 0; $i -lt 120 -and (Test-Path -LiteralPath $extractDir); $i++) {
        Start-Sleep -Milliseconds 500
    }
    if (Test-Path -LiteralPath $extractDir) {
        throw "Portable extraction directory was not cleaned up: $extractDir"
    }
    foreach ($name in @('config.toml', 'policy.toml', 'eir.log')) {
        if (-not (Test-Path -LiteralPath (Join-Path $stateRoot $name) -PathType Leaf)) {
            throw "Portable state disappeared after extraction cleanup: $name"
        }
    }
}
finally {
    [Environment]::SetEnvironmentVariable('RUST_LOG', $ambientRustLog, 'Process')
    if ($eir -and (-not $eir.HasExited)) {
        Stop-Process -Id $eir.Id -ErrorAction SilentlyContinue
    }
    if ((-not $portableService) -and $extractDir) {
        $portableService = Get-Process eir-svc -ErrorAction SilentlyContinue |
            Where-Object {
                $_.StartTime -ge $started -and
                (Test-ProcessPathInDirectory -Path $_.Path -Directory $extractDir)
            } |
            Select-Object -First 1
    }
    if ($portableService -and (-not $portableService.HasExited)) {
        Stop-Process -Id $portableService.Id -ErrorAction SilentlyContinue
    }
    if ($outer) {
        $outer.Refresh()
        if (-not $outer.HasExited) {
            Stop-Process -Id $outer.Id -ErrorAction SilentlyContinue
        }
    }
}

Write-Host 'portable smoke OK' -ForegroundColor Green
