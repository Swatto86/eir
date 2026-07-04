use crate::models::RegistryUndo;
use crate::policy::is_within;
use anyhow::{bail, Result};

/// Registry subtrees Claude is allowed to modify. Anything outside this list is
/// rejected. Matched on component boundaries (see [`registry_key_allowed`]), so a
/// sibling key that merely shares a name prefix (e.g. `…\MicrosoftEvil`) is NOT
/// treated as being under `…\Microsoft`. `Session Manager` was deliberately dropped:
/// it holds boot-time SYSTEM primitives (`PendingFileRenameOperations`, `SubSystems`).
const ALLOWED_KEY_PREFIXES: &[&str] = &[
    "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip",
    "HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Multimedia",
    "HKCU:\\SOFTWARE\\Microsoft",
];

/// Persistence / process-hijack subkeys that are NEVER writable even though they sit
/// under an allowed prefix (the broad `HKCU:\SOFTWARE\Microsoft` grant would otherwise
/// expose them). A registry reset here could plant an autostart entry or a shell/
/// debugger hijack, so they are denied outright — the deny list wins over the allow
/// list. This is defence in depth: `registry_reset` also always requires approval
/// (it is off the auto-execute whitelist), so a human sees every registry write.
const DENIED_KEY_PREFIXES: &[&str] = &[
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\RunOnce",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\RunServices",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer\\Run",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\ShellServiceObjectDelayLoad",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon",
    "HKCU:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options",
    "HKCU:\\SOFTWARE\\Microsoft\\Command Processor",
    // StartupApproved holds the enable/disable state for startup entries. Toggling it is
    // the job of the validated executor::startup path; deny it here so a generic
    // RegistryReset can't reach that closed set via a less-scrutinised approval card.
    "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved",
];

/// PowerShell registry cmdlets glob-expand `*?[]` in `-Path`/`-Name` against real keys
/// at execution time, so a wildcard in a model-supplied key/value would bypass the
/// literal-string gate and hit siblings the deny list protects. Reject any such
/// metacharacter (legitimate registry key paths never contain them) — belt to the
/// `-LiteralPath` braces below.
fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', ']'])
}

/// Whether `key` is safe to modify: contains no glob metacharacters, is under an
/// allowed subtree, and not under any denied persistence subkey. Uses component-
/// boundary matching (via the shared policy `is_within`) rather than raw string
/// prefixes, so `…\MicrosoftEvil` can't masquerade as a child of `…\Microsoft`.
fn registry_key_allowed(key: &str) -> bool {
    if has_glob_meta(key) {
        return false;
    }
    if DENIED_KEY_PREFIXES.iter().any(|d| is_within(key, d)) {
        return false;
    }
    ALLOWED_KEY_PREFIXES.iter().any(|p| is_within(key, p))
}

/// Read the current data of a registry value: `Ok(Some(v))` = present, `Ok(None)` =
/// confirmed absent, `Err` = couldn't determine (transient failure / locked hive).
/// The caller must NOT treat `Err` as "absent" — that would make a later undo DELETE a
/// value that actually existed. `-LiteralPath` stops glob expansion; the property is
/// indexed by the QUOTED name, never interpolated raw.
async fn read_current_value(normalised_key: &str, value_name: &str) -> Result<Option<String>> {
    let path_q = super::powershell::ps_single_quote(normalised_key);
    let name_q = super::powershell::ps_single_quote(value_name);
    // Emit the value, or the sentinel __EIR_NO_VALUE__ when the property/key doesn't
    // exist (so we can tell "absent" from "empty string"); a real read error propagates
    // as a non-zero exit → Err.
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         if (-not (Test-Path -LiteralPath {path_q})) {{ '__EIR_NO_VALUE__' }} else {{ \
           $p = Get-ItemProperty -LiteralPath {path_q}; \
           $v = $p.PSObject.Properties[{name_q}].Value; \
           if ($null -eq $v) {{ '__EIR_NO_VALUE__' }} else {{ [string]$v }} \
         }}"
    );
    let out = super::powershell::run_diagnostic(&script).await?;
    let t = out.trim();
    if t == "__EIR_NO_VALUE__" {
        Ok(None)
    } else {
        Ok(Some(t.to_string()))
    }
}

/// Reset a registry value to the given data, returning a human message and — when the
/// prior value could be snapshotted — a [`RegistryUndo`] for a one-click revert (see
/// [`restore_value`]). `None` undo means the prior value couldn't be read, so no
/// (potentially destructive) undo is offered.
/// `key_path` must use PowerShell-style drive prefix (e.g. `HKLM:\...`).
/// `value_data` is always written as a string; PowerShell coerces DWORD automatically
/// when the existing value is DWORD type.
pub async fn reset_value(
    key_path: &str,
    value_name: &str,
    value_data: &str,
) -> Result<(String, Option<RegistryUndo>)> {
    let normalised = normalise_key(key_path);

    if !registry_key_allowed(&normalised) {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to modify",
            key_path
        );
    }
    // A wildcard in the value name would let -Name fan a write/read across many values.
    if has_glob_meta(value_name) {
        bail!("Registry value name '{value_name}' contains wildcard characters — refusing");
    }

    // Snapshot the prior value first so the write is reversible. If it can't be read,
    // proceed with the fix but offer no undo (rather than a wrong one).
    let undo = match read_current_value(&normalised, value_name).await {
        Ok(prior) => Some(RegistryUndo {
            key_path: normalised.clone(),
            value_name: value_name.to_string(),
            prior_existed: prior.is_some(),
            prior_data: prior,
        }),
        Err(e) => {
            tracing::warn!("Could not snapshot prior registry value (undo disabled): {e}");
            None
        }
    };

    let script = build_reset_script(&normalised, value_name, value_data);
    // Timed, kill_on_drop PowerShell — a bare synchronous `Command::output()` had no
    // timeout, so a locked registry hive could pin a blocking-pool thread forever.
    let msg = super::powershell::run_diagnostic(&script).await?;
    Ok((msg, undo))
}

/// Revert a prior [`reset_value`] using its captured snapshot: restore the previous
/// data if the value existed, otherwise delete the value we created. Re-checks the
/// same allow/deny gate (incl. glob rejection) so an undo can't reach a key a reset
/// couldn't, and uses `-LiteralPath`.
pub async fn restore_value(undo: &RegistryUndo) -> Result<String> {
    let normalised = normalise_key(&undo.key_path);
    if !registry_key_allowed(&normalised) || has_glob_meta(&undo.value_name) {
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
                "Set-ItemProperty -LiteralPath {path_q} -Name {name_q} -Value {data_q} -ErrorAction Stop; \
                 Write-Output 'Restored registry value {safe_name}'"
            )
        }
        _ => format!(
            "Remove-ItemProperty -LiteralPath {path_q} -Name {name_q} -ErrorAction SilentlyContinue; \
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
        "Set-ItemProperty -LiteralPath {path_q} -Name {name_q} -Value {data_q} -ErrorAction Stop; \
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
        // Session Manager was removed from the allowlist (boot-time SYSTEM primitives).
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Session Manager"
        ));
    }

    #[test]
    fn glob_metacharacters_are_rejected() {
        // A wildcard segment would glob-expand under -Path/-Name against real keys,
        // bypassing the literal allow/deny check — reject outright.
        assert!(!registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft*\\Windows\\CurrentVersion\\Run"
        ));
        assert!(!registry_key_allowed("HKCU:\\SOFTWARE\\Microsoft\\*"));
        assert!(!registry_key_allowed("HKCU:\\SOFTWARE\\Microsoft\\Off?ce"));
        assert!(!registry_key_allowed("HKCU:\\SOFTWARE\\Microsoft\\Item[1]"));
        assert!(has_glob_meta("*"));
        assert!(!has_glob_meta("PlainName"));
    }

    #[test]
    fn additional_hkcu_autostart_locations_are_denied() {
        for k in [
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer\\Run\\x",
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders",
            "HKCU:\\SOFTWARE\\Microsoft\\Command Processor",
        ] {
            assert!(!registry_key_allowed(k), "should be denied: {k}");
        }
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
