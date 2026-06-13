use anyhow::Result;

pub async fn uninstall(package_name: &str) -> Result<String> {
    let safe_name = package_name.replace('\'', "''");
    // Try PackageManagement first. Fall back to registry uninstall string — avoids Win32_Product
    // which triggers MSI reconfiguration on every installed app during enumeration.
    let script = format!(
        r#"$pkg = Get-Package -Name '{safe_name}' -ErrorAction SilentlyContinue
if ($pkg) {{
    $pkg | Uninstall-Package -Force -ErrorAction Stop
    Write-Output 'Uninstalled via PackageManagement: {safe_name}'
}} else {{
    $key = Get-ChildItem @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    ) -ErrorAction SilentlyContinue |
    Get-ItemProperty -ErrorAction SilentlyContinue |
    Where-Object {{ $_.DisplayName -eq '{safe_name}' }} |
    Select-Object -First 1
    if (-not $key) {{
        Write-Output 'Package not found: {safe_name}'
        return
    }}
    $cmd = if ($key.QuietUninstallString) {{ $key.QuietUninstallString }} else {{ $key.UninstallString }}
    if (-not $cmd) {{
        Write-Output 'No uninstall command found for: {safe_name}'
        return
    }}
    if ($cmd -match 'msiexec') {{
        $guid = [regex]::Match($cmd, '\{{[0-9A-Fa-f\-]+\}}').Value
        if ($guid) {{
            Start-Process msiexec -ArgumentList "/x $guid /quiet /norestart" -Wait -ErrorAction Stop
            Write-Output 'Uninstalled MSI package: {safe_name}'
        }} else {{
            Write-Output 'MSI product code not found in uninstall command for: {safe_name}'
        }}
    }} else {{
        Write-Output 'Non-MSI silent uninstall not supported for: {safe_name}'
    }}
}}"#
    );
    super::powershell::run_diagnostic(&script).await
}
