//! Inventory of installed Windows applications, independent of any package manager.
//! Reads the Add/Remove Programs registry keys (HKLM, Wow6432Node, per-user HKCU/HKU)
//! so the autonomous updater can see apps that winget and friends do not report.
//!
//! This deliberately mirrors the `startup_scan` PowerShell pattern: a small, bounded
//! external enumeration + JSON deserialization, rather than linking extra registry
//! features into the `windows` crate.

use crate::updater::proc;
use crate::updater::winget_parse::is_noise;
use serde::Deserialize;

const LIST_TIMEOUT: std::time::Duration = proc::LIST;

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

/// Enumerate installed applications from the registry Uninstall keys. Returns a list of
/// `(display_name, display_version)` pairs, filtered for noise and deduplicated by
/// name (lowercase). Best-effort: per-user hives only appear if loaded.
pub async fn list_installed() -> Vec<(String, String)> {
    let script = r#"
$ErrorActionPreference='SilentlyContinue'
$out = New-Object System.Collections.ArrayList
function AddUninstall($root,$path){
  $k="Registry::$root\$path"
  $p=Get-ItemProperty -LiteralPath $k -ErrorAction SilentlyContinue
  if($null -eq $p){return}
  $n=[string]$p.DisplayName
  $v=[string]$p.DisplayVersion
  if([string]::IsNullOrWhiteSpace($n)){return}
  if($p.SystemComponent -eq 1){return}
  [void]$out.Add([pscustomobject]@{name=$n.Trim();version=$v.Trim()})
}
$cv='SOFTWARE\Microsoft\Windows\CurrentVersion'
AddUninstall 'HKEY_LOCAL_MACHINE' "$cv\Uninstall"
AddUninstall 'HKEY_LOCAL_MACHINE' 'SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
AddUninstall 'HKEY_CURRENT_USER' "$cv\Uninstall"
Get-ChildItem 'Registry::HKEY_USERS' -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '^S-1-5-21-' } |
  ForEach-Object { AddUninstall 'HKEY_USERS' ($_.PSChildName + '\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall') }
$out | ConvertTo-Json -Compress
"#;
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let (_code, out) = proc::run_capped_cmd(cmd, LIST_TIMEOUT).await;
    parse_inventory(&out)
}

/// Pure: parse the JSON emitted by the inventory script, drop noise and empty versions,
/// and deduplicate by lowercase name (first wins).
fn parse_inventory(text: &str) -> Vec<(String, String)> {
    let entries: Vec<RawEntry> = serde_json::from_str(text).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        let name = e.name.trim().to_string();
        let version = e.version.trim().to_string();
        if name.is_empty() || version.is_empty() || is_noise(&name) {
            continue;
        }
        let key = name.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        out.push((name, version));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inventory_json_and_drops_noise_and_dupes() {
        let json = r#"[
{"name":"7-Zip","version":"25.01"},
{"name":"  Driver Pack  ","version":"1.0"},
{"name":"7-Zip","version":"24.00"},
{"name":"Eir","version":"0.27.5"}
]"#;
        let apps = parse_inventory(json);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0, "7-Zip");
        assert_eq!(apps[0].1, "25.01");
    }

    #[test]
    fn empty_and_invalid_json_returns_empty() {
        assert!(parse_inventory("").is_empty());
        assert!(parse_inventory("not json").is_empty());
    }
}
