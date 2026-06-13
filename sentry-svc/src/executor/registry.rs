use anyhow::{bail, Result};

/// Registry paths Claude is allowed to modify. Anything outside this list is rejected.
const ALLOWED_KEY_PREFIXES: &[&str] = &[
    "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip",
    "HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Session Manager",
    "HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Multimedia",
    "HKCU:\\SOFTWARE\\Microsoft",
];

/// Reset a registry value to the given data.
/// `key_path` must use PowerShell-style drive prefix (e.g. `HKLM:\...`).
/// `value_data` is always written as a string; PowerShell coerces DWORD automatically
/// when the existing value is DWORD type.
pub fn reset_value(key_path: &str, value_name: &str, value_data: &str) -> Result<String> {
    // Normalise alternate forms (registry editor uses HKEY_LOCAL_MACHINE\...)
    let normalised = key_path
        .replace("HKEY_LOCAL_MACHINE\\", "HKLM:\\")
        .replace("HKEY_CURRENT_USER\\", "HKCU:\\")
        .replace("HKEY_LOCAL_MACHINE/", "HKLM:\\")
        .replace("HKEY_CURRENT_USER/", "HKCU:\\");

    if !ALLOWED_KEY_PREFIXES
        .iter()
        .any(|p| normalised.starts_with(p))
    {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to modify",
            key_path
        );
    }

    // Escape single-quotes in values to prevent injection
    let safe_path = normalised.replace('\'', "''");
    let safe_name = value_name.replace('\'', "''");
    let safe_data = value_data.replace('\'', "''");

    let script = format!(
        "Set-ItemProperty -Path '{safe_path}' -Name '{safe_name}' -Value '{safe_data}' -ErrorAction Stop; \
         Write-Output \"Set '{safe_path}/{safe_name}' = '{safe_data}'\""
    );

    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if out.status.success() {
        Ok(stdout.trim().to_string())
    } else {
        bail!("Registry set failed: {stderr}")
    }
}
