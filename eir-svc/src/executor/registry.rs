use crate::models::RegistryUndo;
use crate::policy::is_within;
use anyhow::{bail, Result};

/// Registry subtrees Claude is allowed to modify. Anything outside this list is
/// rejected. Matched on component boundaries (see [`registry_key_allowed`]), so a
/// sibling key that merely shares a name prefix (e.g. `…\MicrosoftEvil`) is NOT
/// treated as being under `…\Microsoft`.
const ALLOWED_KEY_PREFIXES: &[&str] = &[
    "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip",
    "HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Session Manager",
    "HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Multimedia",
    "HKCU:\\SOFTWARE\\Microsoft",
];

/// Persistence / process-hijack subkeys that are NEVER writable even though they sit
/// under an allowed prefix (the broad `HKCU:\SOFTWARE\Microsoft` grant would otherwise
/// expose them). A registry reset here could plant an autostart entry or a debugger
/// hijack, so they are denied outright — the deny list wins over the allow list.
const DENIED_KEY_PREFIXES: &[&str] = &[
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\RunOnce",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\RunServices",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options",
];

/// Whether `key` is safe to modify: under an allowed subtree, and not under any
/// denied persistence subkey. Uses component-boundary matching (via the shared
/// policy `is_within`) rather than raw string prefixes, so `…\MicrosoftEvil` can't
/// masquerade as a child of `…\Microsoft`.
fn registry_key_allowed(key: &str) -> bool {
    if DENIED_KEY_PREFIXES.iter().any(|d| is_within(key, d)) {
        return false;
    }
    ALLOWED_KEY_PREFIXES.iter().any(|p| is_within(key, p))
}

/// Read the current data of a registry value, or `None` if the key/value does not
/// exist. Used to snapshot the prior state before an overwrite so the change is
/// reversible. The key is assumed already normalised + gated by the caller.
async fn read_current_value(normalised_key: &str, value_name: &str) -> Option<String> {
    let path_q = super::powershell::ps_single_quote(normalised_key);
    let name_q = super::powershell::ps_single_quote(value_name);
    // Index the property by the QUOTED name (never interpolate value_name raw — it is
    // model-controlled). Emit the value, or the sentinel __EIR_NO_VALUE__ when the
    // property/key doesn't exist, so we can tell "absent" from "empty string".
    let script = format!(
        "$ErrorActionPreference='Stop'; try {{ \
           $p = Get-ItemProperty -Path {path_q} -Name {name_q}; \
           $v = $p.PSObject.Properties[{name_q}].Value; \
           if ($null -eq $v) {{ '__EIR_NO_VALUE__' }} else {{ [string]$v }} \
         }} catch {{ '__EIR_NO_VALUE__' }}"
    );
    match super::powershell::run_diagnostic(&script).await {
        Ok(out) => {
            let t = out.trim();
            if t == "__EIR_NO_VALUE__" || t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Reset a registry value to the given data, returning a human message and a
/// [`RegistryUndo`] snapshot of the value BEFORE the write so the change can be
/// reverted (see [`restore_value`]).
/// `key_path` must use PowerShell-style drive prefix (e.g. `HKLM:\...`).
/// `value_data` is always written as a string; PowerShell coerces DWORD automatically
/// when the existing value is DWORD type.
pub async fn reset_value(
    key_path: &str,
    value_name: &str,
    value_data: &str,
) -> Result<(String, RegistryUndo)> {
    let normalised = normalise_key(key_path);

    if !registry_key_allowed(&normalised) {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to modify",
            key_path
        );
    }

    // Snapshot the prior value first so the write is reversible.
    let prior = read_current_value(&normalised, value_name).await;
    let undo = RegistryUndo {
        key_path: normalised.clone(),
        value_name: value_name.to_string(),
        prior_existed: prior.is_some(),
        prior_data: prior,
    };

    let script = build_reset_script(&normalised, value_name, value_data);
    // Timed, kill_on_drop PowerShell — a bare synchronous `Command::output()` had no
    // timeout, so a locked registry hive could pin a blocking-pool thread forever.
    let msg = super::powershell::run_diagnostic(&script).await?;
    Ok((msg, undo))
}

/// Revert a prior [`reset_value`] using its captured snapshot: restore the previous
/// data if the value existed, otherwise delete the value we created. Re-checks the
/// same allow/deny gate so an undo can't reach a key a reset couldn't.
pub async fn restore_value(undo: &RegistryUndo) -> Result<String> {
    let normalised = normalise_key(&undo.key_path);
    if !registry_key_allowed(&normalised) {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to restore",
            undo.key_path
        );
    }
    let path_q = super::powershell::ps_single_quote(&normalised);
    let name_q = super::powershell::ps_single_quote(&undo.value_name);
    let safe_name = undo.value_name.replace('\'', "''");
    let script = match (undo.prior_existed, &undo.prior_data) {
        (true, Some(data)) => {
            let data_q = super::powershell::ps_single_quote(data);
            format!(
                "Set-ItemProperty -Path {path_q} -Name {name_q} -Value {data_q} -ErrorAction Stop; \
                 Write-Output 'Restored registry value {safe_name}'"
            )
        }
        _ => format!(
            "Remove-ItemProperty -Path {path_q} -Name {name_q} -ErrorAction SilentlyContinue; \
             Write-Output 'Removed registry value {safe_name} (it did not exist before)'"
        ),
    };
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
    let path_q = super::powershell::ps_single_quote(normalised_path);
    let name_q = super::powershell::ps_single_quote(value_name);
    let data_q = super::powershell::ps_single_quote(value_data);
    // For the human-readable confirmation, embed the quote-doubled name inside a
    // single-quoted literal (never a double-quoted string).
    let safe_name = value_name.replace('\'', "''");
    format!(
        "Set-ItemProperty -Path {path_q} -Name {name_q} -Value {data_q} -ErrorAction Stop; \
         Write-Output 'Set registry value {safe_name}'"
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
    fn allowlist_matches_on_component_boundaries_not_string_prefix() {
        // Genuine children of an allowed subtree pass.
        assert!(registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer"
        ));
        assert!(registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Parameters"
        ));
        // Case / hive-alias forms still resolve.
        assert!(registry_key_allowed(
            "hkey_current_user\\software\\microsoft\\office"
        ));

        // Sibling keys that merely share a NAME prefix must be rejected — this is
        // the B1 bug: raw `starts_with` accepted these.
        assert!(!registry_key_allowed("HKCU:\\SOFTWARE\\MicrosoftEvil\\x"));
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\TcpipRogue\\x"
        ));
        // Entirely outside any allowed subtree.
        assert!(!registry_key_allowed("HKLM:\\SOFTWARE\\Classes\\x"));
    }

    #[test]
    fn allowlist_denies_persistence_subkeys_under_a_broad_grant() {
        // Run / RunOnce / Winlogon / IFEO sit under the broad HKCU:\SOFTWARE\Microsoft
        // grant but must be denied — the deny list wins over the allow list.
        assert!(!registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run"
        ));
        assert!(!registry_key_allowed(
            "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce\\Evil"
        ));
        assert!(!registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon"
        ));
        assert!(!registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options\\x.exe"
        ));
        // But a sibling that only shares a name prefix with a denied key is still
        // allowed (it isn't actually a persistence location).
        assert!(registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Runtime"
        ));
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
