<#
.SYNOPSIS
  Show a real Windows message-box dialog for the "noticed" e2e spec to detect.

.DESCRIPTION
  eir-ui's screen watcher (eir-ui/src/screen_watch.rs) polls every 2s for a
  visible `#32770` dialog whose title/text reads like an error. This script
  produces a real one: it starts a detached, hidden-console PowerShell process
  that blocks on [System.Windows.Forms.MessageBox]::Show(...), which is
  exactly the kind of native dialog a crashed app leaves on screen.

  There is no OK-click automation and nothing here waits for the dialog to be
  dismissed by a user — MessageBox.Show blocks its whole process, so the
  caller ends the dialog by killing THIS SCRIPT'S OWN PID (printed to
  stdout), never by window title or executable name. That keeps it from ever
  touching a window that isn't this fixture's.

.OUTPUTS
  The started process's PID, alone on the first line of stdout.
#>
[CmdletBinding()]
param(
    [string]$Title = 'Test Application Error',
    [string]$Text = 'The test application could not start. (eir-e2e-fixture)'
)
$ErrorActionPreference = 'Stop'

function Escape-SingleQuoted([string]$s) {
    $s -replace "'", "''"
}

$inner = @"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
[System.Windows.Forms.MessageBox]::Show('$(Escape-SingleQuoted $Text)', '$(Escape-SingleQuoted $Title)', [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Error) | Out-Null
"@

$bytes = [System.Text.Encoding]::Unicode.GetBytes($inner)
$encoded = [Convert]::ToBase64String($bytes)

$proc = Start-Process -FilePath 'powershell.exe' -ArgumentList @(
    '-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', '-EncodedCommand', $encoded
) -PassThru -WindowStyle Hidden

Write-Output $proc.Id
