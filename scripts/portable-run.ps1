#Requires -Version 5.1
[CmdletBinding()]
param(
    [switch]$SelfTest,
    [string]$SelfTestRoot
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function New-DeleteOnCloseSentinel {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Path)

    $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite,
        $share,
        1,
        [IO.FileOptions]::DeleteOnClose
    )
    $bytes = [Text.Encoding]::UTF8.GetBytes('running')
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
    $stream
}

function New-PortableMutex {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Name)

    $created = $false
    $mutex = [Threading.Mutex]::new($true, $Name, [ref]$created)
    [pscustomobject]@{ Mutex = $mutex; Created = $created }
}

function Copy-DefaultIfMissing {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination
    )

    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        return
    }

    $temporary = "$Destination.new-$([guid]::NewGuid().ToString('N'))"
    try {
        [IO.File]::Copy($Source, $temporary, $false)
        try {
            [IO.File]::Move($temporary, $Destination)
        }
        catch [IO.IOException] {
            if (-not (Test-Path -LiteralPath $Destination -PathType Leaf)) {
                throw
            }
        }
    }
    finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force
        }
    }
}

if ($SelfTest) {
    if ([string]::IsNullOrWhiteSpace($SelfTestRoot)) {
        throw 'SelfTestRoot is required for the portable runner self-test'
    }
    $testSentinel = Join-Path $SelfTestRoot 'eir-portable.running'
    $testHandle = New-DeleteOnCloseSentinel -Path $testSentinel
    if (-not (Test-Path -LiteralPath $testSentinel -PathType Leaf)) {
        throw 'Delete-on-close sentinel was not visible while held'
    }
    $testHandle.Dispose()
    if (Test-Path -LiteralPath $testSentinel) {
        throw 'Delete-on-close sentinel survived its owner'
    }

    $testName = "Local\EirPortableSelfTest-$PID-$([guid]::NewGuid().ToString('N'))"
    $first = New-PortableMutex -Name $testName
    $second = New-PortableMutex -Name $testName
    try {
        if ((-not $first.Created) -or $second.Created) {
            throw 'Portable runner mutex did not reject a second instance'
        }
    }
    finally {
        $second.Mutex.Dispose()
        if ($first.Created) {
            $first.Mutex.ReleaseMutex()
        }
        $first.Mutex.Dispose()
    }

    $defaults = Join-Path $SelfTestRoot 'defaults'
    $state = Join-Path $SelfTestRoot 'state'
    [void](New-Item -ItemType Directory -Path $defaults, $state)
    $source = Join-Path $defaults 'config.toml'
    $destination = Join-Path $state 'config.toml'
    [IO.File]::WriteAllText($source, 'default')
    Copy-DefaultIfMissing -Source $source -Destination $destination
    [IO.File]::WriteAllText($destination, 'customised')
    Copy-DefaultIfMissing -Source $source -Destination $destination
    if ([IO.File]::ReadAllText($destination) -ne 'customised') {
        throw 'Portable defaults overwrote persistent user state'
    }

    Write-Host 'portable runner self-test OK' -ForegroundColor Green
    return
}

$instance = New-PortableMutex -Name 'Local\EirPortable'
if (-not $instance.Created) {
    $instance.Mutex.Dispose()
    Add-Type -AssemblyName System.Windows.Forms
    [void][Windows.Forms.MessageBox]::Show(
        'Eir portable is already running in this Windows session.',
        'Eir',
        [Windows.Forms.MessageBoxButtons]::OK,
        [Windows.Forms.MessageBoxIcon]::Information
    )
    exit 2
}

$sentinel = Join-Path $PSScriptRoot 'eir-portable.running'
$sentinelHandle = $null
$service = $null
$ui = $null
$exitCode = 1
$pipeName = '\\.\pipe\EirSvcPortable-{0}' -f [guid]::NewGuid().ToString('N')

try {
    $localAppData = [Environment]::GetFolderPath(
        [Environment+SpecialFolder]::LocalApplicationData
    )
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        throw 'Windows did not provide the local application data directory'
    }
    $stateRoot = Join-Path $localAppData 'EirPortable'
    [void](New-Item -ItemType Directory -Path $stateRoot -Force)
    $stateItem = Get-Item -LiteralPath $stateRoot -Force
    if (($stateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'Portable state directory must not be a reparse point'
    }
    Copy-DefaultIfMissing `
        -Source (Join-Path $PSScriptRoot 'config.toml') `
        -Destination (Join-Path $stateRoot 'config.toml')
    Copy-DefaultIfMissing `
        -Source (Join-Path $PSScriptRoot 'policy.toml') `
        -Destination (Join-Path $stateRoot 'policy.toml')

    $sentinelHandle = New-DeleteOnCloseSentinel -Path $sentinel
    $quotedSentinel = '"{0}"' -f $sentinel
    $quotedStateRoot = '"{0}"' -f $stateRoot
    $service = Start-Process -FilePath (Join-Path $PSScriptRoot 'eir-svc.exe') `
        -ArgumentList @('portable', $quotedSentinel, $pipeName, $quotedStateRoot) `
        -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    $service.Refresh()
    if ($service.HasExited) {
        throw "Portable foreground service exited with code $($service.ExitCode)"
    }

    $env:EIR_PORTABLE = '1'
    $env:EIR_PORTABLE_PIPE = $pipeName
    $ui = Start-Process -FilePath (Join-Path $PSScriptRoot 'eir.exe') -PassThru
    $ui.WaitForExit()
    $exitCode = $ui.ExitCode
}
finally {
    if ($sentinelHandle) {
        $sentinelHandle.Dispose()
    }
    if ($service) {
        $service.Refresh()
        if ((-not $service.HasExited) -and (-not $service.WaitForExit(45000))) {
            Stop-Process -Id $service.Id -ErrorAction SilentlyContinue
            throw 'Portable foreground service did not stop cleanly'
        }
    }
    $instance.Mutex.ReleaseMutex()
    $instance.Mutex.Dispose()
}

exit $exitCode
