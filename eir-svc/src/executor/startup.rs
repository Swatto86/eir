//! Enable/disable a logon startup entry via the Windows "StartupApproved" flag — the
//! same mechanism as Task Manager's Startup tab. Fully reversible: it writes a 12-byte
//! REG_BINARY value (`0x02…` = enabled, `0x03…` = disabled); nothing is ever deleted.
//!
//! The `location` selector is a CLOSED SET mapped to a hard-coded key here — the caller
//! never supplies a raw registry path — and any user SID is validated before use.

use anyhow::{bail, Result};

const APPROVED_BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

/// A SID is safe to interpolate only if it's a real interactive-user SID
/// (`S-1-5-21-…`) whose remainder after the fixed prefix is digits and hyphens — never a
/// raw path fragment.
fn valid_sid(sid: &str) -> bool {
    const PREFIX: &str = "S-1-5-21-";
    sid.len() > PREFIX.len()
        && sid.starts_with(PREFIX)
        && sid[PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
}

/// PowerShell `-Name` can glob-expand `*?[]`; reject a value name containing any, so a
/// crafted name can't fan the write across sibling values (mirrors registry.rs).
fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', ']'])
}

/// Map the closed-set `location` (+ validated SID for user hives) to the exact
/// StartupApproved key. Pure and unit-tested — this is the trust boundary that keeps the
/// write inside the Explorer\StartupApproved subtree.
///
/// Keys are provider-qualified with the `Registry::` prefix, because `HKEY_USERS` has no
/// default PowerShell drive (unlike `HKLM:`/`HKCU:`), so a bare `HKEY_USERS\…` path would
/// not resolve. `Registry::` works for every hive, including the per-user ones we target.
pub(crate) fn approved_key(location: &str, hive: &str) -> Result<String> {
    match location {
        "machine_run" => Ok(format!(r"Registry::HKEY_LOCAL_MACHINE\{APPROVED_BASE}\Run")),
        "common_startup_folder" => Ok(format!(
            r"Registry::HKEY_LOCAL_MACHINE\{APPROVED_BASE}\StartupFolder"
        )),
        "user_run" if valid_sid(hive) => {
            Ok(format!(r"Registry::HKEY_USERS\{hive}\{APPROVED_BASE}\Run"))
        }
        "user_startup_folder" if valid_sid(hive) => Ok(format!(
            r"Registry::HKEY_USERS\{hive}\{APPROVED_BASE}\StartupFolder"
        )),
        _ => bail!("Unknown or invalid startup location '{location}' (hive '{hive}')"),
    }
}

/// Set a startup entry enabled/disabled. `name` is the StartupApproved value name (the
/// Run value name or the `.lnk` filename).
pub async fn set_enabled(name: &str, location: &str, hive: &str, enable: bool) -> Result<String> {
    if has_glob_meta(name) {
        bail!("Startup entry name '{name}' contains wildcard characters — refusing");
    }
    let key = approved_key(location, hive)?;
    let key_q = super::powershell::ps_single_quote(&key);
    let name_q = super::powershell::ps_single_quote(name);
    // 12-byte REG_BINARY: byte[0] 0x02 = enabled, 0x03 = disabled (Task Manager format);
    // the trailing bytes are a "disabled since" timestamp Windows fills lazily — zeros are
    // accepted and mean "unknown", which is fine.
    let first = if enable { "0x02" } else { "0x03" };
    let safe_name = name.replace('\'', "''");
    let verb = if enable { "Enabled" } else { "Disabled" };
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         $k={key_q}; \
         if (-not (Test-Path -LiteralPath $k)) {{ New-Item -Path $k -Force | Out-Null }}; \
         $b=[byte[]]({first},0,0,0,0,0,0,0,0,0,0,0); \
         Set-ItemProperty -LiteralPath $k -Name {name_q} -Value $b -Type Binary; \
         Write-Output '{verb} startup entry {safe_name}'"
    );
    super::powershell::run_diagnostic(&script).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_key_maps_closed_set_and_validates_sid() {
        assert_eq!(
            approved_key("machine_run", "").unwrap(),
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
        );
        assert_eq!(
            approved_key("common_startup_folder", "").unwrap(),
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder"
        );
        let uk = approved_key("user_run", "S-1-5-21-1-2-3-1001").unwrap();
        assert!(uk.starts_with(r"Registry::HKEY_USERS\S-1-5-21-1-2-3-1001\"));
        assert!(uk.ends_with(r"StartupApproved\Run"));

        // A user location with a bad/empty SID is rejected (no raw-path injection).
        assert!(approved_key("user_run", "").is_err());
        assert!(approved_key("user_run", "S-1-5-18").is_err());
        assert!(approved_key("user_run", r"..\..\evil").is_err());
        // Unknown selector is rejected.
        assert!(approved_key("anything_else", "").is_err());
    }

    #[test]
    fn glob_names_are_rejected() {
        assert!(has_glob_meta("Disc*ord"));
        assert!(!has_glob_meta("Discord"));
    }
}
