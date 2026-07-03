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
pub async fn reset_value(key_path: &str, value_name: &str, value_data: &str) -> Result<String> {
    let normalised = normalise_key(key_path);

    // Registry paths are case-insensitive, so compare case-folded — otherwise a
    // legitimate `hklm:\system\...` would be wrongly rejected.
    let lower = normalised.to_lowercase();
    if !ALLOWED_KEY_PREFIXES
        .iter()
        .any(|p| lower.starts_with(&p.to_lowercase()))
    {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to modify",
            key_path
        );
    }

    let script = build_reset_script(&normalised, value_name, value_data);
    // Timed, kill_on_drop PowerShell — a bare synchronous `Command::output()` had no
    // timeout, so a locked registry hive could pin a blocking-pool thread forever.
    super::powershell::run_diagnostic(&script).await
}

/// Normalise alternate key forms (the registry editor uses `HKEY_LOCAL_MACHINE\…`)
/// to the PowerShell drive prefix. Case-insensitive on the hive name so
/// `hkey_local_machine\…` normalises too.
fn normalise_key(key_path: &str) -> String {
    let mut out = key_path.to_string();
    for (from, to) in [
        ("HKEY_LOCAL_MACHINE\\", "HKLM:\\"),
        ("HKEY_CURRENT_USER\\", "HKCU:\\"),
        ("HKEY_LOCAL_MACHINE/", "HKLM:\\"),
        ("HKEY_CURRENT_USER/", "HKCU:\\"),
    ] {
        if out.len() >= from.len() && out[..from.len()].eq_ignore_ascii_case(from) {
            out = format!("{to}{}", &out[from.len()..]);
            break;
        }
    }
    out
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

    #[test]
    fn normalise_key_folds_hive_case_and_slashes() {
        assert_eq!(
            normalise_key("HKEY_LOCAL_MACHINE\\SYSTEM\\X"),
            "HKLM:\\SYSTEM\\X"
        );
        // Mixed-case hive name still normalises.
        assert_eq!(
            normalise_key("hkey_current_user/Software/Y"),
            "HKCU:\\Software/Y"
        );
        // An already-normalised path is left untouched.
        assert_eq!(normalise_key("HKLM:\\SYSTEM\\Z"), "HKLM:\\SYSTEM\\Z");
    }
}
