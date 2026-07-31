use anyhow::{bail, Result};

pub async fn uninstall(package_name: &str) -> Result<String> {
    validate_package_name(package_name)?;
    let script = uninstall_script(package_name);
    super::powershell::run_diagnostic(&script).await
}

fn uninstall_script(package_name: &str) -> String {
    let safe_name = package_name.replace('\'', "''");
    // Prefer an exact registry-backed MSI so its process exit code is observable.
    // PackageManagement remains the fallback for exact non-registry packages.
    format!(
        r#"$roots = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
)
function Get-ExactPackages {{
    @(Get-Package -Name '{safe_name}' -ErrorAction SilentlyContinue |
      Where-Object {{ $_.Name -eq '{safe_name}' }})
}}
function Get-ExactRegistryEntries {{
    @(Get-ChildItem $roots -ErrorAction SilentlyContinue |
      Get-ItemProperty -ErrorAction SilentlyContinue |
      Where-Object {{ $_.DisplayName -eq '{safe_name}' }})
}}

$packages = @(Get-ExactPackages)
$keys = @(Get-ExactRegistryEntries)
if ($packages.Count -gt 1 -or $keys.Count -gt 1) {{
    throw 'Ambiguous package name: {safe_name}'
}}
if ($packages.Count -eq 0 -and $keys.Count -eq 0) {{
    throw 'Package not found: {safe_name}'
}}

if ($keys.Count -eq 1) {{
    $cmd = if ($keys[0].QuietUninstallString) {{
        $keys[0].QuietUninstallString
    }} else {{
        $keys[0].UninstallString
    }}
    if (-not $cmd) {{
        throw 'No uninstall command found for: {safe_name}'
    }}
    if ($cmd -notmatch '(?i)msiexec(?:\.exe)?') {{
        throw 'Non-MSI silent uninstall is not supported for: {safe_name}'
    }}
    $guid = [regex]::Match(
        $cmd,
        '\{{[0-9A-Fa-f]{{8}}-[0-9A-Fa-f]{{4}}-[0-9A-Fa-f]{{4}}-[0-9A-Fa-f]{{4}}-[0-9A-Fa-f]{{12}}\}}'
    ).Value
    if (-not $guid) {{
        throw 'MSI product code not found for: {safe_name}'
    }}
    $msiexec = Join-Path $env:SystemRoot 'System32\msiexec.exe'
    $process = Start-Process -FilePath $msiexec `
        -ArgumentList @('/x', $guid, '/quiet', '/norestart') `
        -Wait -PassThru -ErrorAction Stop
    if ($process.ExitCode -notin @(0, 1641, 3010)) {{
        throw "MSI uninstall failed with exit code $($process.ExitCode)"
    }}
}} else {{
    $packages[0] | Uninstall-Package -Force -ErrorAction Stop
}}

$remainingPackages = @(Get-ExactPackages)
$remainingKeys = @(Get-ExactRegistryEntries)
if ($remainingPackages.Count -ne 0 -or $remainingKeys.Count -ne 0) {{
    throw 'Package is still installed after the uninstall command: {safe_name}'
}}
Write-Output 'Uninstalled package: {safe_name}'"#
    )
}

fn validate_package_name(package_name: &str) -> Result<()> {
    // Get-Package -Name glob-expands `*?[]` even inside a single-quoted literal
    // (globbing is cmdlet-level, not string interpolation), so `*` would match
    // every installed package and pipe them to Uninstall-Package -Force. Refuse
    // wildcards up front — mirrors the has_glob_meta guard in process.rs/tasks.rs.
    if package_name.contains(['*', '?', '[', ']']) {
        bail!("Package name '{package_name}' contains wildcard characters — refusing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_package_names_are_refused() {
        // Get-Package -Name glob-expands even inside a single-quoted literal, so a
        // wildcard would match many packages and pipe them all to Uninstall-Package.
        for bad in ["*", "foo*", "foo?", "foo[bar]", "*foo"] {
            let err = validate_package_name(bad).unwrap_err();
            assert!(err.to_string().contains("wildcard"), "{bad}: {err}");
        }
    }

    #[test]
    fn exact_package_names_are_accepted() {
        assert!(validate_package_name("TotallyFake'Package").is_ok());
    }

    #[test]
    fn uninstall_script_fails_closed_and_verifies_the_effect() {
        let script = uninstall_script("Example");
        assert!(script.contains("throw 'Package not found"));
        assert!(script.contains("-PassThru"));
        assert!(script.contains("3010"));
        assert!(script.contains("still installed"));
        assert!(script.contains("Ambiguous"));
        assert!(script.contains("No uninstall command"));
        assert!(script.contains("Non-MSI"));
    }

    #[tokio::test]
    async fn missing_package_is_reported_as_a_failure() {
        let name = format!("EirMissingPackage-{}", std::process::id());
        let error = uninstall(&name).await.unwrap_err().to_string();
        assert!(error.contains("Package not found"), "{error}");
    }
}
