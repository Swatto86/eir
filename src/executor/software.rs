use anyhow::Result;

pub async fn uninstall(package_name: &str) -> Result<String> {
    let safe_name = package_name.replace('\'', "''");
    // Try Get-Package (PackageManagement) first; fall back to Win32_Product for MSI apps.
    let script = format!(
        r#"$pkg = Get-Package -Name '{safe_name}' -ErrorAction SilentlyContinue
if ($pkg) {{
    $pkg | Uninstall-Package -Force -ErrorAction Stop
    Write-Output 'Uninstalled via PackageManagement: {safe_name}'
}} else {{
    $msi = Get-WmiObject Win32_Product -Filter "Name='${safe_name}'" -ErrorAction SilentlyContinue
    if ($msi) {{
        $msi.Uninstall() | Out-Null
        Write-Output 'Uninstalled via Win32_Product: {safe_name}'
    }} else {{
        Write-Output 'Package not found: {safe_name}'
    }}
}}"#
    );
    super::powershell::run_diagnostic(&script).await
}
