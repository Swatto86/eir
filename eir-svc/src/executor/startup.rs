//! Enable/disable a logon startup entry via the Windows "StartupApproved" flag — the
//! same mechanism as Task Manager's Startup tab. Fully reversible: it writes a 12-byte
//! REG_BINARY value (`0x02…` = enabled, `0x03…` = disabled); nothing is ever deleted.
//!
//! The `location` selector is a CLOSED SET mapped to a hard-coded key here — the caller
//! never supplies a raw registry path — and any user SID is validated before use.

use anyhow::{bail, Context, Result};
use windows::core::PWSTR;
use windows::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
    Security::{Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_USER},
    System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
};

const APPROVED_BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

/// A SID is safe to interpolate only if it's a real interactive-user SID
/// (`S-1-5-21-…`) whose remainder after the fixed prefix is digits and hyphens — never a
/// raw path fragment.
pub(crate) fn valid_sid(sid: &str) -> bool {
    const PREFIX: &str = "S-1-5-21-";
    sid.len() > PREFIX.len()
        && sid.starts_with(PREFIX)
        && sid[PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
}

/// SID of the user attached to the active interactive session. The service uses this
/// both when listing per-user startup entries and immediately before changing one.
pub(crate) fn active_user_sid() -> Result<String> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        bail!("No active interactive user session");
    }

    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }
        .context("Cannot query the active interactive user token")?;

    let result = (|| {
        let mut bytes = 0;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes) };
        if bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
            bail!("Cannot read the active interactive user's SID");
        }
        let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
        let mut info = vec![0usize; words];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(info.as_mut_ptr().cast()),
                bytes,
                &mut bytes,
            )
        }
        .context("Cannot read the active interactive user's SID")?;
        let user = unsafe { &*info.as_ptr().cast::<TOKEN_USER>() };

        let mut string_sid = PWSTR::null();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string_sid) }
            .context("Cannot format the active interactive user's SID")?;
        let sid = unsafe { string_sid.to_string() };
        unsafe {
            let _ = LocalFree(HLOCAL(string_sid.0.cast()));
        }
        let sid = sid.context("Active interactive user SID is not valid UTF-16")?;
        if !valid_sid(&sid) {
            bail!("Active session does not belong to a supported interactive user");
        }
        Ok(sid)
    })();

    unsafe {
        let _ = CloseHandle(token);
    }
    result
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
        // The Wow6432Node Run key's approved state lives under Run32 (StartupApproved
        // itself is not WOW64-redirected).
        "machine_run32" => Ok(format!(
            r"Registry::HKEY_LOCAL_MACHINE\{APPROVED_BASE}\Run32"
        )),
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

fn build_set_enabled_script(key: &str, name: &str, enable: bool, source_guard: &str) -> String {
    let key_q = super::powershell::ps_single_quote(key);
    let name_q = super::powershell::ps_single_quote(name);
    let first = if enable { "0x02" } else { "0x03" };
    let safe_name = name.replace('\'', "''");
    let verb = if enable { "Enabled" } else { "Disabled" };
    format!(
        "$ErrorActionPreference='Stop'; \
         {source_guard} \
         $k={key_q}; \
         if (-not (Test-Path -LiteralPath $k)) {{ New-Item -Path $k -Force | Out-Null }}; \
         $b=[byte[]]({first},0,0,0,0,0,0,0,0,0,0,0); \
         Set-ItemProperty -LiteralPath $k -Name {name_q} -Value $b -Type Binary; \
         $actual = (Get-ItemProperty -LiteralPath $k -ErrorAction Stop).PSObject.Properties[{name_q}].Value; \
         if ($null -eq $actual -or $actual.Length -lt 1 -or $actual[0] -ne {first}) {{ \
           throw 'Startup state did not change to the requested value' }}; \
         Write-Output '{verb} startup entry {safe_name}'"
    )
}

fn validate_user_scope(location: &str, hive: &str, active_sid: &str) -> Result<()> {
    if matches!(
        location,
        "user_run" | "user_startup_folder" | "scheduled_task"
    ) && !hive.eq_ignore_ascii_case(active_sid)
    {
        bail!("Startup entry belongs to a different interactive user");
    }
    Ok(())
}

fn build_source_guard(location: &str, hive: &str, name: &str) -> Result<String> {
    let name_q = super::powershell::ps_single_quote(name);
    let stale = "throw 'Startup entry is stale or missing; scan again'";
    let registry_key = match location {
        "machine_run" => Some(
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
                .to_string(),
        ),
        "machine_run32" => Some(
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run"
                .to_string(),
        ),
        "user_run" if valid_sid(hive) => Some(format!(
            r"Registry::HKEY_USERS\{hive}\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
        )),
        _ => None,
    };
    if let Some(key) = registry_key {
        let key_q = super::powershell::ps_single_quote(&key);
        return Ok(format!(
            "$source = (Get-ItemProperty -LiteralPath {key_q} -ErrorAction Stop).PSObject.Properties[{name_q}]; \
             if ($null -eq $source) {{ {stale} }};"
        ));
    }

    if matches!(location, "common_startup_folder" | "user_startup_folder") {
        if name.is_empty() || name.contains(['\\', '/', ':']) || matches!(name, "." | "..") {
            bail!("Startup shortcut name must be a single local filename");
        }
        let base = if location == "common_startup_folder" {
            "$base = Join-Path $env:ProgramData 'Microsoft\\Windows\\Start Menu\\Programs\\StartUp';"
                .to_string()
        } else if valid_sid(hive) {
            let profile_key = format!(
                r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{hive}"
            );
            let profile_q = super::powershell::ps_single_quote(&profile_key);
            format!(
                "$profile = (Get-ItemProperty -LiteralPath {profile_q} -Name 'ProfileImagePath' -ErrorAction Stop).ProfileImagePath; \
                 if ([string]::IsNullOrWhiteSpace($profile)) {{ {stale} }}; \
                 $base = Join-Path ([Environment]::ExpandEnvironmentVariables([string]$profile)) \
                   'AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup';"
            )
        } else {
            bail!("Invalid user SID for startup folder");
        };
        return Ok(format!(
            "{base} $source = Join-Path $base {name_q}; \
             if (-not (Test-Path -LiteralPath $source -PathType Leaf -ErrorAction Stop)) {{ {stale} }};"
        ));
    }

    bail!("Unknown startup location '{location}'")
}

/// Set a startup entry enabled/disabled. `name` is a StartupApproved value name (Run
/// value or `.lnk` filename), or a full task path for `scheduled_task`.
pub async fn set_enabled(name: &str, location: &str, hive: &str, enable: bool) -> Result<String> {
    if has_glob_meta(name) {
        bail!("Startup entry name '{name}' contains wildcard characters — refusing");
    }
    if matches!(
        location,
        "user_run" | "user_startup_folder" | "scheduled_task"
    ) {
        let active_sid = active_user_sid()?;
        validate_user_scope(location, hive, &active_sid)?;
    }
    if location == "scheduled_task" {
        return if enable {
            super::tasks::enable(name).await
        } else {
            super::tasks::disable(name).await
        };
    }
    let key = approved_key(location, hive)?;
    let source_guard = build_source_guard(location, hive, name)?;
    // 12-byte REG_BINARY: byte[0] 0x02 = enabled, 0x03 = disabled (Task Manager format);
    // the trailing bytes are a "disabled since" timestamp Windows fills lazily — zeros are
    // accepted and mean "unknown", which is fine.
    let script = build_set_enabled_script(&key, name, enable, &source_guard);
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
            approved_key("machine_run32", "").unwrap(),
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32"
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

    #[test]
    fn startup_script_reads_back_the_binary_state() {
        let guard = build_source_guard("machine_run", "", "Updater").unwrap();
        let script = build_set_enabled_script(
            r"Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
            "Updater",
            true,
            &guard,
        );
        assert!(script.contains("Get-ItemProperty"));
        assert!(script.contains("[0] -ne 0x02"));
        assert!(script.contains("throw 'Startup state did not change"));
        assert!(script.find("Startup entry is stale").unwrap() < script.find("$k=").unwrap());
    }

    #[test]
    fn user_startup_actions_require_the_active_user_sid() {
        let active = "S-1-5-21-1-2-3-1001";
        assert!(validate_user_scope("user_run", active, active).is_ok());
        assert!(validate_user_scope("user_startup_folder", active, active).is_ok());
        assert!(validate_user_scope("scheduled_task", active, active).is_ok());
        assert!(validate_user_scope("user_run", "S-1-5-21-9-8-7-1002", active).is_err());
        assert!(validate_user_scope("scheduled_task", "S-1-5-21-9-8-7-1002", active).is_err());
    }

    #[test]
    fn source_guards_check_the_real_startup_target() {
        let run = build_source_guard("user_run", "S-1-5-21-1-2-3-1001", "Updater").unwrap();
        assert!(run.contains(r"HKEY_USERS\S-1-5-21-1-2-3-1001"));
        assert!(run.contains("PSObject.Properties['Updater']"));
        assert!(run.contains("Startup entry is stale or missing"));

        let folder = build_source_guard("common_startup_folder", "", "Updater.lnk").unwrap();
        assert!(folder.contains("$env:ProgramData"));
        assert!(folder.contains("-PathType Leaf"));
        assert!(build_source_guard("common_startup_folder", "", r"..\escape.lnk").is_err());
    }
}
