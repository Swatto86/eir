<#
.SYNOPSIS
  Refuse to run a native executable Windows cannot prove Microsoft signed.

.DESCRIPTION
  `msedgedriver.exe` is executed by the e2e setup and by the suite itself with
  the developer's privileges. Both the trust chain and the publisher are
  checked: a validly signed binary from someone else is still not the driver.
  The organisation is matched rather than a leaf thumbprint because
  Microsoft's signing certificates rotate.

  Throws on any failure; callers invoke it with `&` and let the error
  propagate.

.PARAMETER Path
  The executable to verify.
#>
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path)

$ErrorActionPreference = 'Stop'

$resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).ProviderPath
$signature = Get-AuthenticodeSignature -LiteralPath $resolved
if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
    throw "refusing unsigned or invalid executable: $resolved ($($signature.Status))"
}
if ($signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
    throw "refusing executable not signed by Microsoft Corporation: $resolved"
}
