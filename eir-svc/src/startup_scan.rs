//! On-demand Autoruns-style scan of what launches automatically: Run keys (per-user,
//! machine, and the 32-bit Wow6432Node view), Startup-folder shortcuts, one-shot
//! RunOnce keys, `Policies\Explorer\Run` keys, Winlogon Shell/Userinit anomalies,
//! logon-triggered scheduled tasks, and auto-start services with binaries outside
//! `\Windows\`. Each entry carries the launched binary's Authenticode signer (or file
//! CompanyName) — deterministic, never AI-authored. An optional bounded AI call then
//! explains each entry in plain English (what it is, where it likely came from) and
//! classifies it keep / optional / unnecessary — advisory only; it triggers nothing.
//!
//! Running as LocalSystem, `HKCU` is the SYSTEM hive, so per-user entries are read from
//! each loaded interactive-user hive under `HKEY_USERS\S-1-5-21-…`. Toggling is offered
//! only where a reversible mechanism exists (StartupApproved flags, task enable/disable);
//! everything else is listed report-only.

use crate::ai::client::AiClient;
use crate::models::CallUsage;
use anyhow::{Context, Result};
use eir_proto::StartupEntryView;
use serde::Deserialize;
use std::collections::HashMap;

/// Server-side toggle info for one entry, kept out of the wire (the wire carries only an
/// opaque id). Reconstructs the `StartupSet` (or `TaskEnable`/`TaskDisable`) action for
/// a UI enable/disable click.
pub struct StartupToggle {
    pub name: String,
    /// Closed-set selector: machine_run | machine_run32 | user_run | user_startup_folder
    /// | common_startup_folder | scheduled_task.
    pub location: String,
    /// User SID for the `user_*` locations, empty otherwise.
    pub hive: String,
    /// Full `\Folder\Name` task path for `scheduled_task`, empty otherwise.
    pub task_path: String,
}

pub struct StartupScanResult {
    pub entries: Vec<StartupEntryView>,
    pub targets: HashMap<String, StartupToggle>,
    /// AI-classify call usage, if a classify call was made.
    pub usage: Option<CallUsage>,
}

/// Cap on entries rendered / sent to the classifier. The script emits the toggleable
/// launch points first and the report-only bulk (services) last, so truncation sheds
/// the least actionable rows.
const MAX_ENTRIES: usize = 100;

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    command: String,
    /// Display location selector (see `StartupEntryView.location`).
    #[serde(default)]
    location: String,
    #[serde(default)]
    sid: String,
    /// Tolerates both number and string forms: PowerShell argument binding can turn an
    /// inline `-1` into the string `"-1"` (observed live), and a strict `i64` here made
    /// the WHOLE array parse fail — an empty scan with no error.
    #[serde(default, rename = "approvedByte", deserialize_with = "int_or_string")]
    approved_byte: i64,
    /// "enabled" | "disabled" for locations with their own state (tasks/services);
    /// empty = derive from `approved_byte`.
    #[serde(default)]
    state: String,
    /// Full `\Folder\Name` path for scheduled tasks, empty otherwise.
    #[serde(default, rename = "taskPath")]
    task_path: String,
    /// Authenticode signer CN of the launched binary (empty = unsigned/unknown).
    #[serde(default)]
    signer: String,
    /// VersionInfo CompanyName of the launched binary (fallback identity).
    #[serde(default)]
    company: String,
}

/// Deserialize an i64 that may arrive as a JSON number or a numeric string (see the
/// `approved_byte` field note). Unparseable → -1 (the "absent" default).
fn int_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        I(i64),
        S(String),
    }
    Ok(match IntOrString::deserialize(d)? {
        IntOrString::I(v) => v,
        IntOrString::S(s) => s.trim().parse().unwrap_or(-1),
    })
}

/// Decode the StartupApproved first byte into "enabled". Absent (`-1`) means Windows'
/// default = enabled; otherwise the low bit set means disabled (0x02/0x06 enabled,
/// 0x03/0x07 disabled). Pure, unit-tested.
pub fn decode_enabled(approved_byte: i64) -> bool {
    approved_byte < 0 || (approved_byte & 1) == 0
}

/// An entry is enabled from its explicit state where the location has one
/// (tasks/services), else from the StartupApproved byte. Pure, unit-tested.
fn entry_enabled(state: &str, approved_byte: i64) -> bool {
    match state {
        "" => decode_enabled(approved_byte),
        s => s == "enabled",
    }
}

/// Deterministic "this is a Microsoft/Windows component" flag for the UI's hide
/// filter, from the signer CN or file CompanyName. Pure, unit-tested.
fn is_microsoft(signer: &str, company: &str) -> bool {
    let ms = |s: &str| s.trim().to_ascii_lowercase().starts_with("microsoft");
    ms(signer) || ms(company)
}

/// Map the display location (+ SID / task path) to the closed-set toggle selector used
/// by `executor::startup` / `executor::tasks`. Returns `None` for report-only locations
/// (RunOnce, policy keys, Winlogon, services) — those entries stay view-only.
fn to_toggle(display: &str, sid: &str, task_path: &str) -> Option<(String, String, String)> {
    match display {
        "hklm_run" => Some(("machine_run".into(), String::new(), String::new())),
        "hklm_run32" => Some(("machine_run32".into(), String::new(), String::new())),
        "hkcu_run" => Some(("user_run".into(), sid.to_string(), String::new())),
        "startup_folder" => Some(("user_startup_folder".into(), sid.to_string(), String::new())),
        "common_startup_folder" => {
            Some(("common_startup_folder".into(), String::new(), String::new()))
        }
        "scheduled_task" if !task_path.is_empty() => Some((
            "scheduled_task".into(),
            String::new(),
            task_path.to_string(),
        )),
        _ => None,
    }
}

/// The enumeration script. Reads fixed machine keys, each loaded interactive-user hive,
/// the task scheduler, and the service list; resolves each entry's launched binary and
/// its Authenticode signer / CompanyName (cached per path); emits a compact JSON array.
/// No untrusted interpolation — it reads only fixed key paths and system providers.
const ENUM_SCRIPT: &str = r#"
$ErrorActionPreference='SilentlyContinue'
$results = New-Object System.Collections.ArrayList
$sigCache = @{}
$ws = New-Object -ComObject WScript.Shell
function TargetOf($cmd){
  if([string]::IsNullOrWhiteSpace($cmd)){return $null}
  $c=[Environment]::ExpandEnvironmentVariables($cmd.Trim())
  if($c.StartsWith('\\')){return $null}
  if($c.StartsWith('"')){ $i=$c.IndexOf('"',1); if($i -gt 1){ $p=$c.Substring(1,$i-1); if(-not $p.StartsWith('\\') -and (Test-Path -LiteralPath $p)){return $p} } return $null }
  $ix=$c.IndexOf('.exe',[System.StringComparison]::OrdinalIgnoreCase)
  if($ix -ge 0){ $p=$c.Substring(0,$ix+4); if(Test-Path -LiteralPath $p){return $p} }
  $first=($c -split ' ')[0]
  if($first -and (Test-Path -LiteralPath $first)){return $first}
  return $null
}
function SigOf($cmd,$isLnk){
  $path=$null
  if($isLnk){ try{$path=$ws.CreateShortcut($cmd).TargetPath}catch{} }
  else { $path=TargetOf $cmd }
  if($path -and $path.StartsWith('\\')){ return @('','') }
  if(-not $path -or -not (Test-Path -LiteralPath $path)){ return @('','') }
  if($sigCache.ContainsKey($path)){ return $sigCache[$path] }
  $cn=''
  $sig=Get-AuthenticodeSignature -LiteralPath $path -ErrorAction SilentlyContinue
  if($sig -and $sig.SignerCertificate){
    $m=[regex]::Match($sig.SignerCertificate.Subject,'CN=("[^"]+"|[^,]+)')
    if($m.Success){$cn=$m.Groups[1].Value.Trim('"')}
  }
  $co=''
  try{$co=[string](Get-Item -LiteralPath $path -ErrorAction SilentlyContinue).VersionInfo.CompanyName}catch{}
  $r=@($cn,$co)
  $sigCache[$path]=$r
  return $r
}
function Emit($name,$cmd,$loc,$sid,$ab,$state,$taskPath,$isLnk){
  $s=SigOf $cmd $isLnk
  [void]$results.Add([pscustomobject]@{name=$name;command=[string]$cmd;location=$loc;sid=$sid;approvedByte=[int]$ab;state=$state;taskPath=$taskPath;signer=$s[0];company=$s[1]})
}
function Approved($root,$sub,$name){
  $k="Registry::$root\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\$sub"
  $p=Get-ItemProperty -LiteralPath $k -Name $name -ErrorAction SilentlyContinue
  if($null -eq $p){return -1}
  $v=$p.$name
  if($null -eq $v){return -1}
  return [int]$v[0]
}
function RunLike($root,$sid,$loc,$keyPath,$approvedSub){
  $k="Registry::$root\$keyPath"
  $p=Get-ItemProperty -LiteralPath $k -ErrorAction SilentlyContinue
  if($p){ foreach($pr in $p.PSObject.Properties){ if($pr.Name -in @('PSPath','PSParentPath','PSChildName','PSDrive','PSProvider','PSComputerName')){continue}
    $ab= if($approvedSub){ Approved $root $approvedSub $pr.Name } else { -1 }
    Emit $pr.Name ([string]$pr.Value) $loc $sid $ab '' '' $false } }
}
$cv='SOFTWARE\Microsoft\Windows\CurrentVersion'
RunLike 'HKEY_LOCAL_MACHINE' '' 'hklm_run' "$cv\Run" 'Run'
RunLike 'HKEY_LOCAL_MACHINE' '' 'hklm_run32' "SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\Run" 'Run32'
RunLike 'HKEY_LOCAL_MACHINE' '' 'run_once' "$cv\RunOnce" ''
RunLike 'HKEY_LOCAL_MACHINE' '' 'policies_run' "$cv\Policies\Explorer\Run" ''
$us=Get-ChildItem 'Registry::HKEY_USERS' -ErrorAction SilentlyContinue | Where-Object { $_.PSChildName -match '^S-1-5-21-' -and $_.PSChildName -notmatch '_Classes$' }
foreach($u in $us){
  $sid=$u.PSChildName
  RunLike "HKEY_USERS\$sid" $sid 'hkcu_run' "$cv\Run" 'Run'
  RunLike "HKEY_USERS\$sid" $sid 'run_once' "$cv\RunOnce" ''
  RunLike "HKEY_USERS\$sid" $sid 'policies_run' "$cv\Policies\Explorer\Run" ''
  $pip=(Get-ItemProperty -LiteralPath "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sid" -Name ProfileImagePath -ErrorAction SilentlyContinue).ProfileImagePath
  if($pip){
    $sf=Join-Path $pip 'AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup'
    if(Test-Path -LiteralPath $sf){ foreach($l in Get-ChildItem -LiteralPath $sf -Filter *.lnk -ErrorAction SilentlyContinue){
      $ab=Approved "HKEY_USERS\$sid" 'StartupFolder' $l.Name
      Emit $l.Name $l.FullName 'startup_folder' $sid $ab '' '' $true } }
  }
}
$csf='C:\ProgramData\Microsoft\Windows\Start Menu\Programs\StartUp'
if(Test-Path -LiteralPath $csf){ foreach($l in Get-ChildItem -LiteralPath $csf -Filter *.lnk -ErrorAction SilentlyContinue){
  $ab=Approved 'HKEY_LOCAL_MACHINE' 'StartupFolder' $l.Name
  Emit $l.Name $l.FullName 'common_startup_folder' '' $ab '' '' $true } }
$wl=Get-ItemProperty -LiteralPath 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon' -ErrorAction SilentlyContinue
if($wl){
  $shell=[string]$wl.Shell
  if($shell -and $shell.Trim().ToLower() -ne 'explorer.exe'){ Emit 'Winlogon Shell (non-default)' $shell 'winlogon' '' -1 'enabled' '' $false }
  $ui=[string]$wl.Userinit
  if($ui -and $ui.Trim().TrimEnd(',').ToLower() -ne 'c:\windows\system32\userinit.exe'){ Emit 'Winlogon Userinit (non-default)' $ui 'winlogon' '' -1 'enabled' '' $false }
}
$ts=Get-ScheduledTask -ErrorAction SilentlyContinue | Where-Object { $_.TaskPath -notlike '\Microsoft\*' }
foreach($t in $ts){
  $lt=$t.Triggers | Where-Object { $_ -and $_.CimClass -and $_.CimClass.CimClassName -match 'LogonTrigger' }
  if(-not $lt){continue}
  $act=$t.Actions | Where-Object { $_.Execute } | Select-Object -First 1
  if(-not $act){continue}
  $cmd=(($act.Execute+' '+$act.Arguments).Trim())
  $state= if($t.State -eq 'Disabled'){'disabled'}else{'enabled'}
  Emit $t.TaskName $cmd 'scheduled_task' '' -1 $state ($t.TaskPath+$t.TaskName) $false
}
$svcs=Get-CimInstance Win32_Service -ErrorAction SilentlyContinue | Where-Object { $_.StartMode -eq 'Auto' -and $_.PathName -and ($_.PathName -notmatch '(?i)^"?[a-z]:\\windows\\') }
foreach($s in $svcs){ Emit $s.DisplayName $s.PathName 'service' '' -1 'enabled' '' $false }
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
/// deterministic listing (locations, signers, enabled state) is returned with empty
/// verdicts.
pub async fn scan(ai: Option<&AiClient>, model: &str) -> Result<StartupScanResult> {
    let json = crate::executor::powershell::run_diagnostic(ENUM_SCRIPT)
        .await
        .context("startup enumeration failed")?;
    let mut raw = parse_entries(&json);
    raw.truncate(MAX_ENTRIES);

    let mut entries = Vec::with_capacity(raw.len());
    let mut targets = HashMap::new();
    for r in &raw {
        let scope = if r.task_path.is_empty() {
            &r.sid
        } else {
            &r.task_path
        };
        let id = format!("{}|{}|{}", r.location, scope, r.name);
        let toggle = to_toggle(&r.location, &r.sid, &r.task_path);
        if let Some((loc, hive, task_path)) = toggle {
            targets.insert(
                id.clone(),
                StartupToggle {
                    name: r.name.clone(),
                    location: loc,
                    hive,
                    task_path,
                },
            );
        }
        let signer = if r.signer.trim().is_empty() {
            r.company.trim().to_string()
        } else {
            r.signer.trim().to_string()
        };
        entries.push(StartupEntryView {
            report_only: !targets.contains_key(&id),
            microsoft: is_microsoft(&r.signer, &r.company),
            id,
            name: r.name.clone(),
            command: r.command.clone(),
            location: r.location.clone(),
            enabled: entry_enabled(&r.state, r.approved_byte),
            verdict: String::new(),
            note: String::new(),
            signer,
        });
    }

    // Optional advisory classify — never blocks the listing on failure.
    let usage = match ai {
        Some(client) if !entries.is_empty() => match classify(client, model, &entries).await {
            Ok((verdicts, usage)) => {
                for (i, e) in entries.iter_mut().enumerate() {
                    if let Some((v, n)) = verdicts.get(&i) {
                        e.verdict = v.clone();
                        e.note = n.chars().take(300).collect();
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

/// Human label for a location selector, used in the classify prompt so the model can
/// weigh how unusual the launch point is.
fn location_label(loc: &str) -> &'static str {
    match loc {
        "hklm_run" => "Run key (all users)",
        "hklm_run32" => "Run key (32-bit, all users)",
        "hkcu_run" => "Run key (per-user)",
        "startup_folder" => "Startup folder (per-user)",
        "common_startup_folder" => "Startup folder (all users)",
        "run_once" => "RunOnce key (one-shot, unusual to persist)",
        "policies_run" => "Policies Run key (GPO/rare, sometimes abused)",
        "winlogon" => "Winlogon Shell/Userinit (system-critical, rarely legitimate to change)",
        "scheduled_task" => "Scheduled task (logon trigger)",
        "service" => "Auto-start service",
        _ => "Unknown",
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

/// Ask the model to classify + explain the entries. Returns index → (verdict, note).
/// Index-based (not id) keeps the prompt compact and parsing robust.
async fn classify(
    ai: &AiClient,
    model: &str,
    entries: &[StartupEntryView],
) -> Result<(HashMap<usize, (String, String)>, Option<CallUsage>)> {
    let mut prompt = String::from(
        "You are explaining Windows auto-run items to a non-technical PC owner. For each \
         numbered entry decide a verdict: \"keep\" (needed or clearly useful), \"optional\" \
         (safe to disable to speed up sign-in), or \"unnecessary\" (updater/bloat/leftover \
         that doesn't need to auto-run). Write a plain-English note of at most 35 words: \
         what the program is AND where it most likely came from (e.g. \"crash reporter \
         installed with the NVIDIA graphics driver\", \"cloud-sync helper, part of Dropbox\"). \
         The signer field is the verified publisher of the launched file — use it to ground \
         the origin; don't contradict it. If the file is unsigned AND the location is marked \
         unusual/abused, say it looks suspicious and worth investigating. Reply with ONLY a \
         JSON array like [{\"i\":0,\"verdict\":\"keep\",\"note\":\"...\"}] — one object per \
         entry. No prose, no markdown.\n\nENTRIES (index. name | location | signer | command):\n",
    );
    for (i, e) in entries.iter().enumerate() {
        let cmd: String = e.command.chars().take(160).collect();
        let signer = if e.signer.is_empty() {
            "unsigned/unknown"
        } else {
            &e.signer
        };
        prompt.push_str(&format!(
            "{i}. {} | {} | {signer} | {cmd}\n",
            e.name,
            location_label(&e.location)
        ));
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
    fn entry_enabled_prefers_explicit_state() {
        assert!(entry_enabled("enabled", 3)); // state wins over byte
        assert!(!entry_enabled("disabled", 2));
        assert!(entry_enabled("", -1)); // no state → byte
        assert!(!entry_enabled("", 3));
    }

    #[test]
    fn is_microsoft_matches_signer_or_company_prefix() {
        assert!(is_microsoft("Microsoft Windows", ""));
        assert!(is_microsoft("", "Microsoft Corporation"));
        assert!(is_microsoft("  microsoft corporation", ""));
        assert!(!is_microsoft("NVIDIA Corporation", "NVIDIA"));
        assert!(!is_microsoft("", ""));
        // "Microsoft-adjacent" names that don't start with it are not folded in.
        assert!(!is_microsoft("Contoso Microsoft Reseller", ""));
    }

    #[test]
    fn to_toggle_maps_display_to_closed_set() {
        assert_eq!(
            to_toggle("hkcu_run", "S-1-5-21-1-2-3-1001", ""),
            Some((
                "user_run".into(),
                "S-1-5-21-1-2-3-1001".into(),
                String::new()
            ))
        );
        assert_eq!(
            to_toggle("hklm_run", "anything", ""),
            Some(("machine_run".into(), String::new(), String::new()))
        );
        assert_eq!(
            to_toggle("hklm_run32", "", ""),
            Some(("machine_run32".into(), String::new(), String::new()))
        );
        assert_eq!(
            to_toggle("startup_folder", "S-1-5-21-9", ""),
            Some((
                "user_startup_folder".into(),
                "S-1-5-21-9".into(),
                String::new()
            ))
        );
        assert_eq!(
            to_toggle("common_startup_folder", "", ""),
            Some(("common_startup_folder".into(), String::new(), String::new()))
        );
        assert_eq!(
            to_toggle("scheduled_task", "", r"\Vendor\Updater"),
            Some((
                "scheduled_task".into(),
                String::new(),
                r"\Vendor\Updater".into()
            ))
        );
        // A task without a path, and report-only locations, stay view-only.
        assert_eq!(to_toggle("scheduled_task", "", ""), None);
        assert_eq!(to_toggle("run_once", "", ""), None);
        assert_eq!(to_toggle("policies_run", "", ""), None);
        assert_eq!(to_toggle("winlogon", "", ""), None);
        assert_eq!(to_toggle("service", "", ""), None);
        assert_eq!(to_toggle("mystery", "", ""), None);
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
            r#"[{"name":"A","location":"hklm_run","approvedByte":-1},{"name":"B","location":"scheduled_task","approvedByte":-1,"state":"disabled","taskPath":"\\V\\B","signer":"NVIDIA Corporation"}]"#,
        );
        assert_eq!(many.len(), 2);
        assert_eq!(many[1].task_path, r"\V\B");
        assert_eq!(many[1].signer, "NVIDIA Corporation");
        assert_eq!(many[1].state, "disabled");
    }

    #[test]
    fn parse_entries_tolerates_string_approved_byte() {
        // Observed live: PowerShell argument binding can emit approvedByte as the
        // string "-1"; a strict i64 made the WHOLE array fail → silent empty scan.
        let rows = parse_entries(
            r#"[{"name":"T","location":"scheduled_task","approvedByte":"-1","state":"enabled","taskPath":"\\T"},
                {"name":"R","location":"hklm_run","approvedByte":2}]"#,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].approved_byte, -1);
        assert_eq!(rows[1].approved_byte, 2);
        // Garbage string falls back to -1 (absent), not a parse failure.
        let junk = parse_entries(r#"[{"name":"X","location":"service","approvedByte":"abc"}]"#);
        assert_eq!(junk[0].approved_byte, -1);
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
