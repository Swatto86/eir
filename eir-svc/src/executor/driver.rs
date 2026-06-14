use anyhow::{bail, Result};

const CRITICAL_DRIVERS: &[&str] = &[
    // Storage
    "storahci", "stornvme", "disk", "iaStorV", "lsilogic", "partmgr",
    // System bus / ACPI
    "acpi", "pci", "acpiex", // Network stack
    "ndis", "tcpip", "netbt", "nsiproxy", // Filesystem / volume
    "ntfs", "volmgr", "volsnap", "dfsc", "mup", // USB
    "usbhub", "usbhub3", // WDF / core kernel helpers
    "wdf01000", "ksecpkg",
];

pub async fn disable(driver_name: &str) -> Result<String> {
    let lower = driver_name.to_lowercase();
    if CRITICAL_DRIVERS.iter().any(|&d| d.to_lowercase() == lower) {
        bail!("Refusing to disable critical driver: {driver_name}");
    }
    let safe_name = driver_name.replace('\'', "''");
    let script = format!(
        "sc.exe config '{safe_name}' start= disabled; \
         if ($LASTEXITCODE -ne 0) {{ throw 'sc.exe failed' }}; \
         Write-Output 'Driver {safe_name} disabled'"
    );
    super::powershell::run_diagnostic(&script).await
}

pub async fn enable(driver_name: &str) -> Result<String> {
    let safe_name = driver_name.replace('\'', "''");
    let script = format!(
        "sc.exe config '{safe_name}' start= demand; \
         if ($LASTEXITCODE -ne 0) {{ throw 'sc.exe failed' }}; \
         Write-Output 'Driver {safe_name} set to manual start'"
    );
    super::powershell::run_diagnostic(&script).await
}
