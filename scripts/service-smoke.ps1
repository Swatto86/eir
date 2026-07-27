#Requires -Version 5
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ServiceBinary
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$serviceName = 'EirSvc'
if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    throw "$serviceName already exists; refusing to alter an existing installation."
}

$repoRoot = Split-Path $PSScriptRoot -Parent
$runnerTemp = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd('\')
$smokeRoot = Join-Path $runnerTemp ("eir-service-smoke-" + [guid]::NewGuid().ToString('N'))
$resolvedSmoke = [IO.Path]::GetFullPath($smokeRoot)
if (-not $resolvedSmoke.StartsWith($runnerTemp + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "Smoke directory escaped RUNNER_TEMP: $resolvedSmoke"
}

New-Item -ItemType Directory -Path $resolvedSmoke | Out-Null
$smokeExe = Join-Path $resolvedSmoke 'eir-svc.exe'

try {
    Copy-Item -LiteralPath (Resolve-Path $ServiceBinary) -Destination $smokeExe
    Copy-Item -LiteralPath (Join-Path $repoRoot 'config.toml.example') -Destination (Join-Path $resolvedSmoke 'config.toml')
    Copy-Item -LiteralPath (Join-Path $repoRoot 'policy.toml') -Destination (Join-Path $resolvedSmoke 'policy.toml')

    & $smokeExe install
    if ($LASTEXITCODE -ne 0) {
        throw "Service installer exited with code $LASTEXITCODE"
    }

    $service = Get-Service -Name $serviceName
    $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))
    $registration = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
    if ($registration.StartName -ne 'LocalSystem') {
        throw "$serviceName runs as '$($registration.StartName)', expected LocalSystem."
    }

    $pipe = [IO.Pipes.NamedPipeClientStream]::new(
        '.',
        $serviceName,
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::None
    )
    $pipe.Connect(30000)
    $reader = [IO.StreamReader]::new($pipe, [Text.UTF8Encoding]::new($false))
    $writer = [IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false))
    $writer.AutoFlush = $true

    $statusTask = $reader.ReadLineAsync()
    if (-not $statusTask.Wait(30000)) {
        throw 'Timed out waiting for service status.'
    }
    $status = $statusTask.Result | ConvertFrom-Json
    if ($status.type -ne 'status' -or $status.protocol_version -lt 1) {
        throw "Invalid service status: $($statusTask.Result)"
    }
    foreach ($capability in @('command_results', 'provider_test')) {
        if ($status.capabilities -notcontains $capability) {
            throw "Service did not advertise '$capability'."
        }
    }

    $requestId = 9001
    $writer.WriteLine((@{
        type = 'toggle_pause'
        request_id = $requestId
    } | ConvertTo-Json -Compress))

    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    $result = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $lineTask = $reader.ReadLineAsync()
        $remaining = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        if (-not $lineTask.Wait($remaining)) {
            break
        }
        if ($null -eq $lineTask.Result) {
            throw 'Service pipe closed before the command result arrived.'
        }
        $message = $lineTask.Result | ConvertFrom-Json
        if ($message.type -eq 'command_result' -and $message.request_id -eq $requestId) {
            $result = $message
            break
        }
    }
    if ($null -eq $result -or -not $result.ok -or [string]::IsNullOrWhiteSpace($result.message)) {
        throw 'Installed service did not apply the correlated smoke command.'
    }

    $reader.Dispose()
    $writer.Dispose()
    $pipe.Dispose()
    Write-Host "Installed LocalSystem service applied a correlated command: $($result.message)"
}
catch {
    $log = Join-Path $resolvedSmoke 'eir.log'
    if (Test-Path -LiteralPath $log) {
        Get-Content -LiteralPath $log -Tail 100 | Write-Host
    }
    throw
}
finally {
    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($service) {
        if ($service.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
            Stop-Service -Name $serviceName -Force -ErrorAction SilentlyContinue
            $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(30))
        }
        $service.Dispose()
        & $smokeExe uninstall
    }
    if (Test-Path -LiteralPath $resolvedSmoke) {
        Remove-Item -LiteralPath $resolvedSmoke -Recurse -Force
    }
}
