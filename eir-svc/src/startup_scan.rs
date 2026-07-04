//! On-demand scan of what launches at logon: Run-key entries (per-user + machine) and
//! Startup-folder shortcuts, with their current enabled/disabled state read from the
//! Windows "StartupApproved" flag. An optional bounded AI call classifies each entry
//! (keep / optional / unnecessary) — advisory only; it triggers nothing.
//!
//! Running as LocalSystem, `HKCU` is the SYSTEM hive, so per-user entries are read from
//! each loaded interactive-user hive under `HKEY_USERS\S-1-5-21-…` instead. Wow6432Node
//! Run and logon scheduled tasks are out of scope for v1 (documented limitation).

use crate::ai::client::AiClient;
use crate::models::CallUsage;
use anyhow::{Context, Result};
use eir_proto::StartupEntryView;
use serde::Deserialize;
use std::collections::HashMap;

/// Server-side toggle info for one entry, kept out of the wire (the wire carries only an
/// opaque id). Reconstructs the `StartupSet` action for a UI enable/disable click.
pub struct StartupToggle {
    pub name: String,
    /// Closed-set approved-key selector: machine_run | user_run | user_startup_folder |
    /// common_startup_folder.
    pub location: String,
    /// User SID for the `user_*` locations, empty otherwise.
    pub hive: String,
}

pub struct StartupScanResult {
    pub entries: Vec<StartupEntryView>,
    pub targets: HashMap<String, StartupToggle>,
    /// AI-classify call usage, if a classify call was made.
    pub usage: Option<CallUsage>,
}

/// Cap on entries sent to the classifier / rendered (startup lists are small).
const MAX_ENTRIES: usize = 60;

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    command: String,
    /// Display location: hklm_run | hkcu_run | startup_folder | common_startup_folder.
    #[serde(default)]
    location: String,
    #[serde(default)]
    sid: String,
    #[serde(default, rename = "approvedByte")]
    approved_byte: i64,
}

/// Decode the StartupApproved first byte into "enabled". Absent (`-1`) means Windows'
/// default = enabled; otherwise the low bit set means disabled (0x02/0x06 enabled,
/// 0x03/0x07 disabled). Pure, unit-tested.
pub fn decode_enabled(approved_byte: i64) -> bool {
    approved_byte < 0 || (approved_byte & 1) == 0
}

/// Map the display location (+ SID) to the closed-set toggle selector used by
/// `executor::startup`. Returns `None` for an unknown location (entry stays view-only).
fn to_toggle(display: &str, sid: &str) -> Option<(String, String)> {
    match display {
        "hklm_run" => Some(("machine_run".into(), String::new())),
        "hkcu_run" => Some(("user_run".into(), sid.to_string())),
        "startup_folder" => Some(("user_startup_folder".into(), sid.to_string())),
        "common_startup_folder" => Some(("common_startup_folder".into(), String::new())),
        _ => None,
    }
}

/// The enumeration script. Reads machine + each loaded interactive-user hive; emits a
/// compact JSON array. No untrusted interpolation — it reads only fixed key paths.
const ENUM_SCRIPT: &str = r#"
$ErrorActionPreference='SilentlyContinue'
$results = New-Object System.Collections.ArrayList
function Approved($root,$sub,$name){
  $k="Registry::$root\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\$sub"
  $p=Get-ItemProperty -LiteralPath $k -Name $name -ErrorAction SilentlyContinue
  if($null -eq $p){return -1}
  $v=$p.$name
  if($null -eq $v){return -1}
  return [int]$v[0]
}
function RunKeys($root,$sid,$loc){
  $k="Registry::$root\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
  $p=Get-ItemProperty -LiteralPath $k -ErrorAction SilentlyContinue
  if($p){ foreach($pr in $p.PSObject.Properties){ if($pr.Name -like 'PS*'){continue}
    $ab=Approved $root 'Run' $pr.Name
    [void]$results.Add([pscustomobject]@{name=$pr.Name;command=[string]$pr.Value;location=$loc;sid=$sid;approvedByte=$ab}) } }
}
RunKeys 'HKEY_LOCAL_MACHINE' '' 'hklm_run'
$us=Get-ChildItem 'Registry::HKEY_USERS' -ErrorAction SilentlyContinue | Where-Object { $_.PSChildName -match '^S-1-5-21-' -and $_.PSChildName -notmatch '_Classes$' }
foreach($u in $us){
  $sid=$u.PSChildName
  RunKeys "HKEY_USERS\$sid" $sid 'hkcu_run'
  $pip=(Get-ItemProperty -LiteralPath "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sid" -Name ProfileImagePath -ErrorAction SilentlyContinue).ProfileImagePath
  if($pip){
    $sf=Join-Path $pip 'AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup'
    if(Test-Path -LiteralPath $sf){ foreach($l in Get-ChildItem -LiteralPath $sf -Filter *.lnk -ErrorAction SilentlyContinue){
      $ab=Approved "HKEY_USERS\$sid" 'StartupFolder' $l.Name
      [void]$results.Add([pscustomobject]@{name=$l.Name;command=$l.FullName;location='startup_folder';sid=$sid;approvedByte=$ab}) } }
  }
}
$csf='C:\ProgramData\Microsoft\Windows\Start Menu\Programs\StartUp'
if(Test-Path -LiteralPath $csf){ foreach($l in Get-ChildItem -LiteralPath $csf -Filter *.lnk -ErrorAction SilentlyContinue){
  $ab=Approved 'HKEY_LOCAL_MACHINE' 'StartupFolder' $l.Name
  [void]$results.Add([pscustomobject]@{name=$l.Name;command=$l.FullName;location='common_startup_folder';sid='';approvedByte=$ab}) } }
ConvertTo-Json -InputObject @($results) -Depth 4 -Compress
"#;

/// Parse the enumeration JSON, tolerating the empty and single-object shapes PowerShell's
/// `ConvertTo-Json` can emit.
fn parse_entries(json: &str) -> Vec<RawEntry> {
    let t = json.trim();
    if t.is_empty() || t == "null" {
        return Vec::new();
    }
    if let Ok(v) = serde_json::from_str::<Vec<RawEntry>>(t) {
        return v;
    }
    if let Ok(one) = serde_json::from_str::<RawEntry>(t) {
        return vec![one];
    }
    Vec::new()
}

/// Run the scan. `ai`/`model` drive the optional classify pass; without a provider the
/// deterministic listing is returned with empty verdicts.
pub async fn scan(ai: Option<&AiClient>, model: &str) -> Result<StartupScanResult> {
    let json = crate::executor::powershell::run_diagnostic(ENUM_SCRIPT)
        .await
        .context("startup enumeration failed")?;
    let mut raw = parse_entries(&json);
    raw.truncate(MAX_ENTRIES);

    let mut entries = Vec::with_capacity(raw.len());
    let mut targets = HashMap::new();
    for r in &raw {
        let id = format!("{}|{}|{}", r.location, r.sid, r.name);
        if let Some((loc, hive)) = to_toggle(&r.location, &r.sid) {
            targets.insert(
                id.clone(),
                StartupToggle {
                    name: r.name.clone(),
                    location: loc,
                    hive,
                },
            );
        }
        entries.push(StartupEntryView {
            id,
            name: r.name.clone(),
            command: r.command.clone(),
            location: r.location.clone(),
            enabled: decode_enabled(r.approved_byte),
            verdict: String::new(),
            note: String::new(),
        });
    }

    // Optional advisory classify — never blocks the listing on failure.
    let usage = match ai {
        Some(client) if !entries.is_empty() => match classify(client, model, &entries).await {
            Ok((verdicts, usage)) => {
                for (i, e) in entries.iter_mut().enumerate() {
                    if let Some((v, n)) = verdicts.get(&i) {
                        e.verdict = v.clone();
                        e.note = n.clone();
                    }
                }
                usage
            }
            Err(e) => {
                tracing::warn!("Startup classify failed (listing still returned): {e}");
                None
            }
        },
        _ => None,
    };

    Ok(StartupScanResult {
        entries,
        targets,
        usage,
    })
}

fn valid_verdict(v: &str) -> String {
    match v {
        "keep" | "optional" | "unnecessary" => v.to_string(),
        _ => "optional".to_string(),
    }
}

#[derive(Deserialize)]
struct Verdict {
    #[serde(default)]
    i: usize,
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    note: String,
}

/// Ask the model to classify the entries. Returns index → (verdict, note). Index-based
/// (not id) keeps the prompt compact and parsing robust.
async fn classify(
    ai: &AiClient,
    model: &str,
    entries: &[StartupEntryView],
) -> Result<(HashMap<usize, (String, String)>, Option<CallUsage>)> {
    let mut prompt = String::from(
        "You are classifying Windows startup programs for a non-technical PC owner. For each \
         numbered entry decide: \"keep\" (needed or clearly useful at startup), \"optional\" \
         (safe to disable to speed boot), or \"unnecessary\" (updater/bloat that doesn't need to \
         auto-run). Reply with ONLY a JSON array like \
         [{\"i\":0,\"verdict\":\"keep\",\"note\":\"...\"}] — one object per entry, verdict one of \
         keep|optional|unnecessary, note a short plain-English \"what this is\" (max 12 words). \
         No prose, no markdown.\n\nENTRIES:\n",
    );
    for (i, e) in entries.iter().enumerate() {
        let cmd: String = e.command.chars().take(160).collect();
        prompt.push_str(&format!("{i}. {} — {cmd}\n", e.name));
    }
    let (text, usage) = ai.complete_text(&prompt, model).await?;
    let arr = extract_json_array(&text).unwrap_or("[]");
    let verdicts: Vec<Verdict> = serde_json::from_str(arr).unwrap_or_default();
    let map = verdicts
        .into_iter()
        .filter(|v| v.i < entries.len())
        .map(|v| (v.i, (valid_verdict(&v.verdict), v.note)))
        .collect();
    Ok((map, usage))
}

/// Extract the first `[` … last `]` span so stray prose around the JSON doesn't break
/// parsing.
fn extract_json_array(s: &str) -> Option<&str> {
    let start = s.find('[')?;
    let end = s.rfind(']')?;
    if end > start {
        Some(&s[start..=end])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_enabled_handles_absent_enabled_and_disabled() {
        assert!(decode_enabled(-1)); // absent → Windows default = enabled
        assert!(decode_enabled(2)); // 0x02 enabled
        assert!(decode_enabled(6)); // 0x06 enabled
        assert!(!decode_enabled(3)); // 0x03 disabled
        assert!(!decode_enabled(7)); // 0x07 disabled
    }

    #[test]
    fn to_toggle_maps_display_to_closed_set() {
        assert_eq!(
            to_toggle("hkcu_run", "S-1-5-21-1-2-3-1001"),
            Some(("user_run".into(), "S-1-5-21-1-2-3-1001".into()))
        );
        assert_eq!(
            to_toggle("hklm_run", "anything"),
            Some(("machine_run".into(), String::new()))
        );
        assert_eq!(
            to_toggle("startup_folder", "S-1-5-21-9"),
            Some(("user_startup_folder".into(), "S-1-5-21-9".into()))
        );
        assert_eq!(
            to_toggle("common_startup_folder", ""),
            Some(("common_startup_folder".into(), String::new()))
        );
        assert_eq!(to_toggle("mystery", ""), None);
    }

    #[test]
    fn parse_entries_tolerates_empty_object_and_array() {
        assert!(parse_entries("").is_empty());
        assert!(parse_entries("null").is_empty());
        let one = parse_entries(
            r#"{"name":"Discord","command":"c:\\d.exe","location":"hkcu_run","sid":"S-1-5-21-1","approvedByte":2}"#,
        );
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "Discord");
        let many = parse_entries(
            r#"[{"name":"A","location":"hklm_run","approvedByte":-1},{"name":"B","location":"hklm_run","approvedByte":3}]"#,
        );
        assert_eq!(many.len(), 2);
    }

    #[test]
    fn extract_json_array_strips_prose() {
        assert_eq!(extract_json_array("noise [1,2] tail"), Some("[1,2]"));
        assert_eq!(extract_json_array("no array here"), None);
    }

    #[test]
    fn valid_verdict_falls_back_to_optional() {
        assert_eq!(valid_verdict("keep"), "keep");
        assert_eq!(valid_verdict("unnecessary"), "unnecessary");
        assert_eq!(valid_verdict("garbage"), "optional");
    }
}
