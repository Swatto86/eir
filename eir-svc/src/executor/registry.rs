use crate::models::{RegistryScalarKind, RegistryUndo};
use crate::policy::is_within;
use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Registry subtrees Claude is allowed to modify. Anything outside this list is
/// rejected. Matched on component boundaries (see [`registry_key_allowed`]), so a
/// sibling key that merely shares a name prefix (e.g. `…\MicrosoftEvil`) is NOT
/// treated as being under `…\Microsoft`. `Session Manager` was deliberately dropped:
/// it holds boot-time SYSTEM primitives (`PendingFileRenameOperations`, `SubSystems`).
const ALLOWED_KEY_PREFIXES: &[&str] = &[
    "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip",
    "HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Multimedia",
];

/// PowerShell registry cmdlets glob-expand `*?[]` in `-Path`/`-Name` against real keys
/// at execution time, so a wildcard in a model-supplied key/value would bypass the
/// literal-string gate and hit siblings the deny list protects. Reject any such
/// metacharacter (legitimate registry key paths never contain them) — belt to the
/// `-LiteralPath` braces below.
fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', ']'])
}

/// Whether `key` is safe to modify: contains no glob metacharacters and is under one
/// of the narrow machine-only subtrees. Uses component-boundary matching (via the
/// shared policy `is_within`) rather than raw string prefixes.
fn registry_key_allowed(key: &str) -> bool {
    if has_glob_meta(key) {
        return false;
    }
    ALLOWED_KEY_PREFIXES.iter().any(|p| is_within(key, p))
}

#[derive(Deserialize)]
struct RegistrySnapshot {
    existed: bool,
    kind: Option<String>,
    data: Option<String>,
}

/// Read the exact scalar type and unexpanded data before any write. A missing value
/// is a valid snapshot, but a missing key, unsupported type, or read error is not.
async fn read_current_value(normalised_key: &str, value_name: &str) -> Result<RegistryUndo> {
    let script = build_snapshot_script(normalised_key, value_name);
    let out = super::powershell::run_diagnostic(&script).await?;
    parse_snapshot_output(normalised_key, value_name, &out)
}

fn build_snapshot_script(normalised_key: &str, value_name: &str) -> String {
    let path_q = super::powershell::ps_single_quote(normalised_key);
    let name_q = super::powershell::ps_single_quote(value_name);
    format!(
        "$ErrorActionPreference='Stop'; \
         $key = Get-Item -LiteralPath {path_q} -ErrorAction Stop; \
         $name = {name_q}; \
         if (-not (@($key.GetValueNames()) -contains $name)) {{ \
           [pscustomobject]@{{ existed = $false; kind = $null; data = $null }} | ConvertTo-Json -Compress \
         }} else {{ \
           $kind = $key.GetValueKind($name).ToString(); \
           if (@('String','ExpandString','DWord','QWord') -notcontains $kind) {{ \
             throw ('Registry value type is not supported for exact undo: ' + $kind) \
           }}; \
           $value = $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); \
           $data = if ($kind -ceq 'DWord') {{ \
             [BitConverter]::ToUInt32([BitConverter]::GetBytes([int32]$value), 0).ToString([Globalization.CultureInfo]::InvariantCulture) \
           }} elseif ($kind -ceq 'QWord') {{ \
             [BitConverter]::ToUInt64([BitConverter]::GetBytes([int64]$value), 0).ToString([Globalization.CultureInfo]::InvariantCulture) \
           }} else {{ [string]$value }}; \
           [pscustomobject]@{{ existed = $true; kind = $kind; data = $data }} | ConvertTo-Json -Compress \
         }}"
    )
}

fn parse_snapshot_output(
    normalised_key: &str,
    value_name: &str,
    output: &str,
) -> Result<RegistryUndo> {
    let snapshot: RegistrySnapshot =
        serde_json::from_str(output.trim()).context("Registry snapshot returned invalid data")?;
    if !snapshot.existed {
        if snapshot.kind.is_some() || snapshot.data.is_some() {
            bail!("Absent registry snapshot unexpectedly contained a type or value");
        }
        return Ok(RegistryUndo {
            key_path: normalised_key.to_string(),
            value_name: value_name.to_string(),
            prior_existed: false,
            prior_kind: None,
            prior_data: None,
            applied_kind: None,
            applied_data: None,
        });
    }

    let kind_name = snapshot
        .kind
        .context("Registry snapshot omitted the existing value's type")?;
    let kind = RegistryScalarKind::from_storage(&kind_name)
        .with_context(|| format!("Unsupported registry value type '{kind_name}'"))?;
    let data = snapshot
        .data
        .context("Registry snapshot omitted the existing value's data")?;
    let data = canonical_data(kind, &data)?;
    Ok(RegistryUndo {
        key_path: normalised_key.to_string(),
        value_name: value_name.to_string(),
        prior_existed: true,
        prior_kind: Some(kind),
        prior_data: Some(data),
        applied_kind: None,
        applied_data: None,
    })
}

/// Reset a registry value while preserving the existing scalar type. If the value is
/// absent it is created as `REG_SZ`. Snapshot failures and unsupported types are fatal
/// and occur before the write, so every successful reset has a durable exact undo.
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
    if has_glob_meta(value_name) {
        bail!("Registry value name '{value_name}' contains wildcard characters — refusing");
    }

    let mut undo = read_current_value(&normalised, value_name)
        .await
        .with_context(|| {
            format!(
                "Could not snapshot registry value '{}\\{}'; refusing to write",
                normalised, value_name
            )
        })?;
    let target_kind = if undo.prior_existed {
        undo.prior_kind
            .context("Existing registry snapshot had no scalar type")?
    } else {
        RegistryScalarKind::String
    };
    let applied_data = canonical_data(target_kind, value_data)?;
    undo.applied_kind = Some(target_kind);
    undo.applied_data = Some(applied_data.clone());

    let script = build_reset_script(&normalised, value_name, target_kind, &applied_data, &undo)?;
    let msg = super::powershell::run_diagnostic(&script).await?;
    Ok((msg, Some(undo)))
}

/// Revert a prior [`reset_value`] using its captured snapshot: restore the previous
/// scalar type and data if the value existed, otherwise delete the value we created.
/// Re-checks the same machine-only allowlist and glob rejection.
pub async fn restore_value(undo: &RegistryUndo) -> Result<String> {
    let normalised = normalise_key(&undo.key_path);
    if !registry_key_allowed(&normalised) || has_glob_meta(&undo.value_name) {
        bail!(
            "Registry path '{}' is not on the safe list — refusing to restore",
            undo.key_path
        );
    }
    let script = build_restore_script(&normalised, undo)?;
    super::powershell::run_diagnostic(&script).await
}

fn build_restore_script(normalised: &str, undo: &RegistryUndo) -> Result<String> {
    let guard = build_applied_value_guard(normalised, undo)?;
    let restore = build_restore_commands(
        normalised,
        undo,
        "Restored registry value did not match its prior value",
    )?;
    let safe_name = undo.value_name.replace('\'', "''");
    let message = if undo.prior_existed {
        format!("Restored registry value {safe_name}")
    } else {
        format!("Removed registry value {safe_name} (it did not exist before)")
    };
    Ok(format!(
        "{guard}; {restore}; Write-Output {}",
        super::powershell::ps_single_quote(&message)
    ))
}

fn build_applied_value_guard(normalised: &str, undo: &RegistryUndo) -> Result<String> {
    let kind = undo
        .applied_kind
        .context("Registry undo is missing Eir's applied value type")?;
    let data = undo
        .applied_data
        .as_deref()
        .context("Registry undo is missing Eir's applied value data")?;
    let canonical = canonical_data(kind, data)?;
    let path_q = super::powershell::ps_single_quote(normalised);
    let name_q = super::powershell::ps_single_quote(&undo.value_name);
    let kind_q = super::powershell::ps_single_quote(kind.as_str());
    let data_q = super::powershell::ps_single_quote(&canonical);
    let changed_q = super::powershell::ps_single_quote(
        "Registry value changed since Eir applied it — refusing to overwrite the newer value",
    );
    let actual_expression = actual_data_expression(kind);
    Ok(format!(
        "$ErrorActionPreference='Stop'; \
         $key = Get-Item -LiteralPath {path_q} -ErrorAction Stop; \
         $name = {name_q}; \
         if (-not (@($key.GetValueNames()) -contains $name)) {{ throw {changed_q} }}; \
         $actualKind = $key.GetValueKind($name).ToString(); \
         if ($actualKind -cne {kind_q}) {{ throw {changed_q} }}; \
         $actual = $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); \
         $actualData = {actual_expression}; \
         if ($actualData -cne {data_q}) {{ throw {changed_q} }}"
    ))
}

fn build_restore_commands(
    normalised: &str,
    undo: &RegistryUndo,
    verification_error: &str,
) -> Result<String> {
    let path_q = super::powershell::ps_single_quote(normalised);
    let name_q = super::powershell::ps_single_quote(&undo.value_name);
    if undo.prior_existed {
        let kind = undo
            .prior_kind
            .context("Registry undo is missing the prior value type")?;
        let data = undo
            .prior_data
            .as_deref()
            .context("Registry undo is missing the prior value data")?;
        build_typed_write_script(normalised, &undo.value_name, kind, data, verification_error)
    } else {
        if undo.prior_kind.is_some() || undo.prior_data.is_some() {
            bail!("Absent registry undo unexpectedly contains a prior type or value");
        }
        let still_exists_q = super::powershell::ps_single_quote(&format!(
            "{verification_error}: value still exists"
        ));
        Ok(format!(
            "if (Test-Path -LiteralPath {path_q} -ErrorAction Stop) {{ \
               $key = Get-Item -LiteralPath {path_q} -ErrorAction Stop; \
               $name = {name_q}; \
               if (@($key.GetValueNames()) -contains $name) {{ \
                 Remove-ItemProperty -LiteralPath {path_q} -Name $name -ErrorAction Stop \
               }}; \
               $key = Get-Item -LiteralPath {path_q} -ErrorAction Stop; \
               if (@($key.GetValueNames()) -contains $name) {{ \
                 throw {still_exists_q} \
               }} \
             }}"
        ))
    }
}

fn build_typed_write_script(
    normalised_path: &str,
    value_name: &str,
    kind: RegistryScalarKind,
    value_data: &str,
    verification_error: &str,
) -> Result<String> {
    let canonical = canonical_data(kind, value_data)?;
    let path_q = super::powershell::ps_single_quote(normalised_path);
    let name_q = super::powershell::ps_single_quote(value_name);
    let expected_q = super::powershell::ps_single_quote(&canonical);
    let kind_q = super::powershell::ps_single_quote(kind.as_str());
    let value_expression = match kind {
        RegistryScalarKind::String | RegistryScalarKind::ExpandString => expected_q.clone(),
        RegistryScalarKind::DWord => {
            format!("([BitConverter]::ToInt32([BitConverter]::GetBytes([uint32]{canonical}), 0))")
        }
        RegistryScalarKind::QWord => {
            format!("([BitConverter]::ToInt64([BitConverter]::GetBytes([uint64]{canonical}), 0))")
        }
    };
    let actual_expression = actual_data_expression(kind);
    let absent_error_q =
        super::powershell::ps_single_quote(&format!("{verification_error}: value is absent"));
    let kind_error_q =
        super::powershell::ps_single_quote(&format!("{verification_error}: type mismatch"));
    let data_error_q =
        super::powershell::ps_single_quote(&format!("{verification_error}: data mismatch"));

    Ok(format!(
        "$ErrorActionPreference='Stop'; \
         New-ItemProperty -LiteralPath {path_q} -Name {name_q} -PropertyType {} -Value {value_expression} -Force -ErrorAction Stop | Out-Null; \
         $key = Get-Item -LiteralPath {path_q} -ErrorAction Stop; \
         $name = {name_q}; \
         if (-not (@($key.GetValueNames()) -contains $name)) {{ throw {absent_error_q} }}; \
         $actualKind = $key.GetValueKind($name).ToString(); \
         if ($actualKind -cne {kind_q}) {{ throw {kind_error_q} }}; \
         $actual = $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); \
         $actualData = {actual_expression}; \
         if ($actualData -cne {expected_q}) {{ throw {data_error_q} }}",
        kind.as_str()
    ))
}

fn actual_data_expression(kind: RegistryScalarKind) -> &'static str {
    match kind {
        RegistryScalarKind::String | RegistryScalarKind::ExpandString => "[string]$actual",
        RegistryScalarKind::DWord => {
            "[BitConverter]::ToUInt32([BitConverter]::GetBytes([int32]$actual), 0).ToString([Globalization.CultureInfo]::InvariantCulture)"
        }
        RegistryScalarKind::QWord => {
            "[BitConverter]::ToUInt64([BitConverter]::GetBytes([int64]$actual), 0).ToString([Globalization.CultureInfo]::InvariantCulture)"
        }
    }
}

fn canonical_data(kind: RegistryScalarKind, value_data: &str) -> Result<String> {
    match kind {
        RegistryScalarKind::String | RegistryScalarKind::ExpandString => Ok(value_data.to_string()),
        RegistryScalarKind::DWord => parse_dword(value_data).map(|value| value.to_string()),
        RegistryScalarKind::QWord => parse_qword(value_data).map(|value| value.to_string()),
    }
}

fn parse_dword(value: &str) -> Result<u32> {
    let value = value.trim();
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => value.parse(),
    };
    parsed.with_context(|| format!("'{value}' is not valid DWORD data"))
}

fn parse_qword(value: &str) -> Result<u64> {
    let value = value.trim();
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    };
    parsed.with_context(|| format!("'{value}' is not valid QWORD data"))
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

/// Build a typed write with exact type/data postchecks. Values only appear inside
/// single-quoted PowerShell literals (with embedded quotes doubled).
fn build_reset_script(
    normalised_path: &str,
    value_name: &str,
    kind: RegistryScalarKind,
    value_data: &str,
    undo: &RegistryUndo,
) -> Result<String> {
    if undo.key_path != normalised_path || undo.value_name != value_name {
        bail!("Registry rollback snapshot does not match the write target");
    }
    let expected_kind = if undo.prior_existed {
        undo.prior_kind
            .context("Existing registry rollback snapshot has no scalar type")?
    } else {
        RegistryScalarKind::String
    };
    if kind != expected_kind {
        bail!("Registry write type does not match the rollback snapshot");
    }
    let applied_data = canonical_data(kind, value_data)?;
    if undo.applied_kind != Some(kind)
        || undo.applied_data.as_deref() != Some(applied_data.as_str())
    {
        bail!("Registry applied-value guard does not match the requested write");
    }

    let write = build_typed_write_script(
        normalised_path,
        value_name,
        kind,
        value_data,
        "Registry value did not match the requested value after write",
    )?;
    let rollback = build_restore_commands(
        normalised_path,
        undo,
        "Registry rollback did not restore the prior value",
    )?;
    let safe_name = value_name.replace('\'', "''");
    Ok(format!(
        "try {{ {write} }} catch {{ \
           $eirWriteError = $_.Exception.Message; \
           try {{ {rollback} }} catch {{ \
             $eirRollbackError = $_.Exception.Message; \
             throw ('Registry write failed and rollback failed: ' + $eirWriteError + '; rollback: ' + $eirRollbackError) \
           }}; \
           throw ('Registry write failed and was rolled back: ' + $eirWriteError) \
         }}; \
         Write-Output 'Set registry value {safe_name}'"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = r"HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters";

    fn absent_undo(value_name: &str, applied_data: &str) -> RegistryUndo {
        RegistryUndo {
            key_path: TEST_KEY.into(),
            value_name: value_name.into(),
            prior_existed: false,
            prior_kind: None,
            prior_data: None,
            applied_kind: Some(RegistryScalarKind::String),
            applied_data: Some(applied_data.into()),
        }
    }

    fn present_undo(
        value_name: &str,
        kind: RegistryScalarKind,
        prior_data: &str,
        applied_data: &str,
    ) -> RegistryUndo {
        RegistryUndo {
            key_path: TEST_KEY.into(),
            value_name: value_name.into(),
            prior_existed: true,
            prior_kind: Some(kind),
            prior_data: Some(prior_data.into()),
            applied_kind: Some(kind),
            applied_data: Some(applied_data.into()),
        }
    }

    #[test]
    fn reset_script_never_double_quotes_untrusted_values() {
        let script = build_reset_script(
            TEST_KEY,
            "$(calc.exe)",
            RegistryScalarKind::String,
            "$(rm -rf)",
            &absent_undo("$(calc.exe)", "$(rm -rf)"),
        )
        .expect("script");
        assert!(!script.contains('"'), "no double-quoted strings: {script}");
        assert!(script.contains("'$(calc.exe)'"));
        assert!(script.contains("'$(rm -rf)'"));
    }

    #[test]
    fn reset_script_doubles_embedded_single_quotes() {
        let script = build_reset_script(
            TEST_KEY,
            "it's",
            RegistryScalarKind::String,
            "va'lue",
            &absent_undo("it's", "va'lue"),
        )
        .expect("script");
        assert!(script.contains("it''s"));
        assert!(script.contains("va''lue"));
        assert!(!script.contains('"'));
    }

    #[test]
    fn reset_scripts_write_and_verify_each_supported_scalar_type() {
        for (kind, data) in [
            (RegistryScalarKind::String, "plain"),
            (RegistryScalarKind::ExpandString, r"%SystemRoot%\Temp"),
            (RegistryScalarKind::DWord, "42"),
            (RegistryScalarKind::QWord, "42"),
        ] {
            let script = build_reset_script(
                TEST_KEY,
                "Setting",
                kind,
                data,
                &present_undo("Setting", kind, "1", data),
            )
            .expect("script");
            assert!(
                script.contains(&format!("-PropertyType {}", kind.as_str())),
                "{script}"
            );
            assert!(script.contains("GetValueKind"), "{script}");
            assert!(script.contains("DoNotExpandEnvironmentNames"), "{script}");
            assert!(script.contains("$actualData -cne"), "{script}");
        }
    }

    #[test]
    fn numeric_data_is_canonical_and_range_checked() {
        let dword = build_reset_script(
            TEST_KEY,
            "Setting",
            RegistryScalarKind::DWord,
            "0xFFFFFFFF",
            &present_undo("Setting", RegistryScalarKind::DWord, "1", "4294967295"),
        )
        .expect("DWORD");
        assert!(dword.contains("[uint32]4294967295"), "{dword}");
        assert!(build_reset_script(
            TEST_KEY,
            "Setting",
            RegistryScalarKind::DWord,
            "4294967296",
            &present_undo("Setting", RegistryScalarKind::DWord, "1", "4294967296"),
        )
        .is_err());
        assert!(build_reset_script(
            TEST_KEY,
            "Setting",
            RegistryScalarKind::QWord,
            "18446744073709551616",
            &present_undo(
                "Setting",
                RegistryScalarKind::QWord,
                "1",
                "18446744073709551616",
            ),
        )
        .is_err());
    }

    #[test]
    fn reset_script_rolls_back_if_write_or_postcheck_fails() {
        let script = build_reset_script(
            TEST_KEY,
            "Setting",
            RegistryScalarKind::DWord,
            "7",
            &present_undo("Setting", RegistryScalarKind::DWord, "42", "7"),
        )
        .expect("script");
        assert!(script.contains("catch"), "{script}");
        assert!(script.contains("-PropertyType DWord"), "{script}");
        assert!(script.contains("[uint32]42"), "{script}");
        assert!(
            script.contains("Registry write failed and was rolled back"),
            "{script}"
        );

        let created = build_reset_script(
            TEST_KEY,
            "NewSetting",
            RegistryScalarKind::String,
            "new",
            &absent_undo("NewSetting", "new"),
        )
        .expect("script");
        assert!(created.contains("catch"), "{created}");
        assert!(created.contains("Remove-ItemProperty"), "{created}");
    }

    #[test]
    fn absent_value_restore_is_idempotent_and_verifies_removal() {
        let script = build_restore_script(
            TEST_KEY,
            &RegistryUndo {
                key_path: String::new(),
                value_name: "Setting".into(),
                prior_existed: false,
                prior_kind: None,
                prior_data: None,
                applied_kind: Some(RegistryScalarKind::String),
                applied_data: Some("new".into()),
            },
        )
        .expect("script");
        assert!(script.contains("Test-Path"));
        assert!(script.contains("Remove-ItemProperty"));
        assert!(!script.contains("SilentlyContinue"));
        assert!(script.contains("value still exists"));
    }

    #[test]
    fn present_value_restore_recreates_the_exact_dword_type() {
        let script = build_restore_script(
            TEST_KEY,
            &RegistryUndo {
                key_path: String::new(),
                value_name: "Setting".into(),
                prior_existed: true,
                prior_kind: Some(crate::models::RegistryScalarKind::DWord),
                prior_data: Some("42".into()),
                applied_kind: Some(RegistryScalarKind::DWord),
                applied_data: Some("7".into()),
            },
        )
        .expect("script");
        assert!(script.contains("-PropertyType DWord"));
        assert!(script.contains("GetValueKind"));
    }

    #[test]
    fn restore_refuses_if_the_current_value_is_not_eirs_applied_value() {
        let script = build_restore_script(
            TEST_KEY,
            &RegistryUndo {
                key_path: String::new(),
                value_name: "Setting".into(),
                prior_existed: true,
                prior_kind: Some(RegistryScalarKind::DWord),
                prior_data: Some("42".into()),
                applied_kind: Some(RegistryScalarKind::DWord),
                applied_data: Some("7".into()),
            },
        )
        .expect("script");
        let guard = script
            .find("changed since Eir applied it")
            .expect("applied-value guard");
        let restore = script.find("New-ItemProperty").expect("restore write");
        assert!(guard < restore, "guard must run before restore: {script}");
        assert!(script.contains("$actualKind -cne 'DWord'"), "{script}");
        assert!(script.contains("$actualData -cne '7'"), "{script}");
    }

    #[test]
    fn legacy_or_inconsistent_undo_is_rejected() {
        let present_without_kind = RegistryUndo {
            key_path: String::new(),
            value_name: "Setting".into(),
            prior_existed: true,
            prior_kind: None,
            prior_data: Some("42".into()),
            applied_kind: Some(RegistryScalarKind::DWord),
            applied_data: Some("7".into()),
        };
        assert!(build_restore_script(TEST_KEY, &present_without_kind,).is_err());

        let absent_with_data = RegistryUndo {
            key_path: String::new(),
            value_name: "Setting".into(),
            prior_existed: false,
            prior_kind: None,
            prior_data: Some("42".into()),
            applied_kind: Some(RegistryScalarKind::String),
            applied_data: Some("new".into()),
        };
        assert!(build_restore_script(TEST_KEY, &absent_with_data,).is_err());
    }

    #[test]
    fn snapshot_preserves_unexpanded_type_and_data() {
        let script = build_snapshot_script(
            r"HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
            "Setting",
        );
        assert!(script.contains("GetValueKind"));
        assert!(script.contains("DoNotExpandEnvironmentNames"));
        assert!(script.contains("'String','ExpandString','DWord','QWord'"));

        let undo = parse_snapshot_output(
            r"HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
            "Setting",
            r#"{"existed":true,"kind":"ExpandString","data":"%SystemRoot%\\Temp"}"#,
        )
        .expect("snapshot");
        assert!(undo.prior_existed);
        assert_eq!(undo.prior_kind, Some(RegistryScalarKind::ExpandString));
        assert_eq!(undo.prior_data.as_deref(), Some(r"%SystemRoot%\Temp"));
        assert!(undo.applied_kind.is_none());
        assert!(undo.applied_data.is_none());
    }

    #[test]
    fn malformed_or_unsupported_snapshot_is_rejected() {
        let parse = |json| {
            parse_snapshot_output(
                r"HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
                "Setting",
                json,
            )
        };
        assert!(parse(r#"{"existed":true,"kind":"Binary","data":"AA=="}"#).is_err());
        assert!(parse(r#"{"existed":true,"kind":"DWord","data":null}"#).is_err());
        assert!(parse(r#"{"existed":false,"kind":"String","data":"unexpected"}"#).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn allowlist_is_machine_only_and_component_bounded() {
        assert!(!registry_key_allowed(
            "HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer"
        ));
        assert!(!registry_key_allowed(
            "hkey_current_user\\software\\microsoft\\office"
        ));
        // Genuine children of a machine allowlisted subtree pass.
        assert!(registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Parameters"
        ));
        assert!(registry_key_allowed(
            "hkey_local_machine\\software\\microsoft\\windows nt\\currentversion\\multimedia\\systemprofile"
        ));

        assert!(!registry_key_allowed("HKCU:\\SOFTWARE\\MicrosoftEvil\\x"));
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\TcpipRogue\\x"
        ));
        assert!(!registry_key_allowed("HKLM:\\SOFTWARE\\Classes\\x"));
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Session Manager"
        ));
    }

    #[test]
    fn glob_metacharacters_are_rejected() {
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip*\\Parameters"
        ));
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip\\*"
        ));
        assert!(!registry_key_allowed(
            "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Off?ce"
        ));
        assert!(has_glob_meta("*"));
        assert!(!has_glob_meta("PlainName"));
    }

    #[test]
    fn normalise_key_folds_hive_case_and_slashes() {
        assert_eq!(
            normalise_key("HKEY_LOCAL_MACHINE\\SYSTEM\\X"),
            "HKLM:\\SYSTEM\\X"
        );
        assert_eq!(
            normalise_key("hkey_current_user/Software/Y"),
            "HKCU:\\Software/Y"
        );
        assert_eq!(normalise_key("HKLM:\\SYSTEM\\Z"), "HKLM:\\SYSTEM\\Z");
    }
}
