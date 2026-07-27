//! Inventory of installed Windows applications, independent of any package manager.
//! Reads the Add/Remove Programs registry keys (HKLM, Wow6432Node, per-user HKCU/HKU)
//! so the autonomous updater can see apps that winget and friends do not report.
//!
//! This deliberately mirrors the `startup_scan` PowerShell pattern: a small, bounded
//! external enumeration + JSON deserialization, rather than linking extra registry
//! features into the `windows` crate.

use crate::updater::proc;
use crate::updater::winget_parse::is_noise;
use crate::updater::{config::valid_app_id, names::app_id};
use serde::Deserialize;

const LIST_TIMEOUT: std::time::Duration = proc::LIST;
const MAX_INVENTORY_APPS: usize = 2_000;
const MAX_RAW_NAME_CHARS: usize = 300;
const MAX_VERSION_CHARS: usize = 100;
const MAX_WARNINGS: usize = 20;
const MAX_WARNING_CHARS: usize = 300;

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct RawInventory {
    #[serde(default)]
    apps: Vec<RawEntry>,
    #[serde(default)]
    warnings: Vec<String>,
}

pub struct InventoryResult {
    pub apps: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

fn bounded_warnings(warnings: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut omitted = 0;
    for warning in warnings {
        let warning: String = warning
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_WARNING_CHARS)
            .collect();
        if warning.is_empty() || !seen.insert(warning.clone()) {
            continue;
        }
        if out.len() < MAX_WARNINGS {
            out.push(warning);
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        out.push(format!(
            "{omitted} more registry inventory warning(s) omitted"
        ));
    }
    out
}

const INVENTORY_SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$out = [System.Collections.Generic.List[object]]::new()
$warnings = [System.Collections.Generic.List[string]]::new()
$maxApps = 2000
$maxNameChars = 300
$maxVersionChars = 100
function Add-Warning([string]$message) {
  if ($warnings.Count -ge 20) { return }
  $message = $message.Trim()
  if ($message.Length -gt 300) { $message = $message.Substring(0,300) }
  if ($message) { $warnings.Add($message) }
}
function Add-UninstallRoot([string]$path) {
  try {
    if (-not (Test-Path -LiteralPath $path -ErrorAction Stop)) { return }
    if ($out.Count -ge $maxApps) {
      Add-Warning 'registry inventory app limit reached'
      return
    }
    $remaining = $maxApps - $out.Count
    Get-ChildItem -LiteralPath $path -ErrorAction Stop |
      Select-Object -First $remaining |
      ForEach-Object {
      try {
        $p = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction Stop
        $n = [string]$p.DisplayName
        $v = [string]$p.DisplayVersion
        if (-not [string]::IsNullOrWhiteSpace($n) -and $p.SystemComponent -ne 1) {
          $n = $n.Trim()
          $v = $v.Trim()
          if ($n.Length -gt $maxNameChars -or $v.Length -gt $maxVersionChars) {
            Add-Warning 'registry product entry exceeded field limits'
          } else {
            $out.Add([pscustomobject]@{name=$n;version=$v})
          }
        }
      } catch {
        Add-Warning ("could not read registry product key: {0}" -f $_.Exception.Message)
      }
    }
  } catch {
    Add-Warning ("could not read {0}: {1}" -f $path,$_.Exception.Message)
  }
}
$cv = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall'
Add-UninstallRoot "Registry::HKEY_LOCAL_MACHINE\$cv"
Add-UninstallRoot 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
Add-UninstallRoot "Registry::HKEY_CURRENT_USER\$cv"
try {
  Get-ChildItem -LiteralPath 'Registry::HKEY_USERS' -Name -ErrorAction Stop |
    Where-Object {
      $_ -match '^S-1-5-21-(?:\d+-){3}\d+$' -or
      $_ -match '^S-1-12-1-(?:\d+-){3}\d+$'
    } |
    ForEach-Object { Add-UninstallRoot "Registry::HKEY_USERS\$_\$cv" }
} catch {
  Add-Warning ("could not enumerate loaded user hives: {0}" -f $_.Exception.Message)
}
[pscustomobject]@{apps=@($out);warnings=@($warnings)} | ConvertTo-Json -Compress -Depth 3
"#;

/// Enumerate installed applications from the registry Uninstall keys. Returns a list of
/// `(display_name, display_version)` pairs, filtered for noise and deduplicated by
/// name (lowercase). Per-user hives only appear if loaded. The script reads only
/// immediate registry children under fixed roots; it never expands install paths.
pub async fn list_installed() -> Result<InventoryResult, String> {
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        INVENTORY_SCRIPT,
    ]);
    let (code, out) = proc::run_capped_cmd(cmd, LIST_TIMEOUT).await;
    parse_inventory(proc::checked_output("registry app inventory", code, &out)?)
}

/// Pure: parse the JSON emitted by the inventory script, drop noise and empty versions,
/// and deduplicate by lowercase name (first wins).
fn parse_inventory(text: &str) -> Result<InventoryResult, String> {
    let inventory: RawInventory =
        serde_json::from_str(text).map_err(|e| format!("invalid registry inventory JSON: {e}"))?;
    if inventory.apps.len() > MAX_INVENTORY_APPS {
        return Err(format!(
            "registry inventory exceeded the {MAX_INVENTORY_APPS}-app limit"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut warnings = inventory.warnings;
    for e in inventory.apps {
        let name = e.name.trim();
        let version = e.version.trim();
        if name.chars().count() > MAX_RAW_NAME_CHARS || version.chars().count() > MAX_VERSION_CHARS
        {
            warnings.push("registry product entry exceeded field limits".to_string());
            continue;
        }
        if name.is_empty() || version.is_empty() || is_noise(name) || !valid_app_id(&app_id(name)) {
            continue;
        }
        let key = name.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        out.push((name.to_string(), version.to_string()));
    }
    Ok(InventoryResult {
        apps: out,
        warnings: bounded_warnings(warnings),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inventory_json_and_drops_noise_and_dupes() {
        let json = r#"{"apps":[
{"name":"7-Zip","version":"25.01"},
{"name":"  Driver Pack  ","version":"1.0"},
{"name":"7-Zip","version":"24.00"},
{"name":"Eir","version":"0.27.5"}
],"warnings":["one hive was inaccessible"]}"#;
        let inventory = parse_inventory(json).expect("parse");
        assert_eq!(inventory.apps.len(), 1);
        assert_eq!(inventory.apps[0].0, "7-Zip");
        assert_eq!(inventory.apps[0].1, "25.01");
        assert_eq!(inventory.warnings, ["one hive was inaccessible"]);
    }

    #[test]
    fn empty_and_invalid_json_are_failures() {
        assert!(parse_inventory("").is_err());
        assert!(parse_inventory("not json").is_err());
    }

    #[test]
    fn oversized_registry_app_id_is_dropped() {
        let json = serde_json::json!({
            "apps": [{"name": "x".repeat(crate::updater::config::MAX_APP_ID_CHARS + 1), "version": "1"}],
            "warnings": []
        })
        .to_string();
        assert!(parse_inventory(&json).expect("parse").apps.is_empty());
    }

    #[test]
    fn inventory_script_reads_product_children_without_filesystem_expansion() {
        assert!(INVENTORY_SCRIPT.contains("Get-ChildItem -LiteralPath $path"));
        assert!(INVENTORY_SCRIPT.contains("Get-ItemProperty -LiteralPath $_.PSPath"));
        assert!(INVENTORY_SCRIPT.contains("Registry::HKEY_USERS"));
        assert!(INVENTORY_SCRIPT.contains("S-1-5-21-"));
        assert!(INVENTORY_SCRIPT.contains("S-1-12-1-"));
        assert!(INVENTORY_SCRIPT.contains("$maxApps = 2000"));
        assert!(INVENTORY_SCRIPT.contains("$maxNameChars = 300"));
        assert!(INVENTORY_SCRIPT.contains("$maxVersionChars = 100"));
        assert!(!INVENTORY_SCRIPT.contains("InstallLocation"));
        assert!(!INVENTORY_SCRIPT.contains("UninstallString"));
        assert!(!INVENTORY_SCRIPT.contains("-Recurse"));
    }

    #[test]
    fn raw_inventory_fields_and_count_are_bounded() {
        let oversized_name = format!("Tool {}", "1.2 ".repeat(MAX_RAW_NAME_CHARS));
        let fields = serde_json::json!({
            "apps": [
                {"name": oversized_name, "version": "1"},
                {"name": "Tool", "version": "1".repeat(MAX_VERSION_CHARS + 1)}
            ],
            "warnings": []
        })
        .to_string();
        let parsed = parse_inventory(&fields).expect("bounded parse");
        assert!(parsed.apps.is_empty());
        assert!(!parsed.warnings.is_empty());

        let apps: Vec<_> = (0..=MAX_INVENTORY_APPS)
            .map(|index| serde_json::json!({"name": format!("Tool {index}"), "version": "1"}))
            .collect();
        let too_many = serde_json::json!({"apps": apps, "warnings": []}).to_string();
        assert!(parse_inventory(&too_many).is_err());
    }

    #[test]
    fn inventory_warnings_are_bounded_and_deduplicated() {
        let mut warnings = vec![
            " duplicate warning ".to_string(),
            "duplicate   warning".to_string(),
            "x".repeat(MAX_WARNING_CHARS + 50),
        ];
        warnings.extend((0..20).map(|index| format!("warning {index}")));
        let warnings = bounded_warnings(warnings);
        assert_eq!(warnings.len(), MAX_WARNINGS + 1);
        assert!(warnings
            .iter()
            .take(MAX_WARNINGS)
            .all(|warning| warning.chars().count() <= MAX_WARNING_CHARS));
        assert_eq!(
            warnings.last().map(String::as_str),
            Some("2 more registry inventory warning(s) omitted")
        );
    }
}
