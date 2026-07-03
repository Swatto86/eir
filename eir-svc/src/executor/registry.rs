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

    let script = build_reset_script(&normalised, value_name, value_data);

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

/// Build the `Set-ItemProperty` script from an already-normalised key path plus the
/// raw value name/data. Kept pure so the injection-safety of the escaping is
/// unit-testable: values are only ever placed inside single-quoted PowerShell
/// literals (with embedded `'` doubled), never a double-quoted string.
fn build_reset_script(normalised_path: &str, value_name: &str, value_data: &str) -> String {
    let safe_path = normalised_path.replace('\'', "''");
    let safe_name = value_name.replace('\'', "''");
    let safe_data = value_data.replace('\'', "''");
    format!(
        "Set-ItemProperty -Path '{safe_path}' -Name '{safe_name}' -Value '{safe_data}' -ErrorAction Stop; \
         Write-Output 'Set registry value {safe_name} at {safe_path}'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_script_never_double_quotes_untrusted_values() {
        // `$(...)` in the value/name must stay a literal — no double-quoted string
        // anywhere in the script (which is where PowerShell would evaluate it).
        let script = build_reset_script(
            "HKCU:\\SOFTWARE\\Microsoft\\Test",
            "$(calc.exe)",
            "$(rm -rf)",
        );
        assert!(!script.contains('"'), "no double-quoted strings: {script}");
        assert!(script.contains("'$(calc.exe)'"));
        assert!(script.contains("'$(rm -rf)'"));
    }

    #[test]
    fn reset_script_doubles_embedded_single_quotes() {
        let script = build_reset_script("HKCU:\\SOFTWARE\\X", "it's", "va'lue");
        assert!(script.contains("it''s"));
        assert!(script.contains("va''lue"));
        assert!(!script.contains('"'));
    }
}
