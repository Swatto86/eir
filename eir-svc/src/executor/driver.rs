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

fn config_script(driver_name: &str, start_mode: &str, expected_start: u32) -> Result<String> {
    if driver_name.is_empty()
        || !driver_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("Invalid driver service name: {driver_name}");
    }
    let safe_name = driver_name.replace('\'', "''");
    Ok(format!(
        "sc.exe config '{safe_name}' start= {start_mode}; \
         if ($LASTEXITCODE -ne 0) {{ throw 'sc.exe failed' }}; \
         $start = Get-ItemPropertyValue -LiteralPath \
           'Registry::HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\{safe_name}' \
           -Name Start -ErrorAction Stop; \
         if ($start -ne {expected_start}) {{ throw 'Driver start mode did not change' }}"
    ))
}

pub async fn disable(driver_name: &str) -> Result<String> {
    let lower = driver_name.to_lowercase();
    if CRITICAL_DRIVERS.iter().any(|&d| d.to_lowercase() == lower) {
        bail!("Refusing to disable critical driver: {driver_name}");
    }
    let script = format!(
        "{}; Write-Output 'Driver {} disabled'",
        config_script(driver_name, "disabled", 4)?,
        driver_name.replace('\'', "''")
    );
    super::powershell::run_diagnostic(&script).await
}

pub async fn enable(driver_name: &str) -> Result<String> {
    let script = format!(
        "{}; Write-Output 'Driver {} set to manual start'",
        config_script(driver_name, "demand", 3)?,
        driver_name.replace('\'', "''")
    );
    super::powershell::run_diagnostic(&script).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_verified_and_names_are_bounded() {
        let script = config_script("example_driver", "demand", 3).unwrap();
        assert!(script.contains("Get-ItemPropertyValue"));
        assert!(script.contains("$start -ne 3"));
        assert!(config_script(r"..\bad", "demand", 3).is_err());
    }
}
