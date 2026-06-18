//! App-update monitoring via winget. Listing runs unelevated; applying an update
//! runs winget elevated through `Start-Process -Verb RunAs` (one UAC prompt) so
//! machine-scope packages can be installed.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

/// CREATE_NO_WINDOW — keep the console-based winget/powershell hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── Per-app notes / ignore list (persisted to %APPDATA%\Eir\app-notes.json) ─

#[derive(Serialize, Deserialize, Clone, Default)]
struct AppNote {
    #[serde(default)]
    ignore: bool,
    #[serde(default)]
    note: String,
}

fn notes_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    let dir = std::path::Path::new(&base).join("Eir");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("app-notes.json"))
}

fn load_notes() -> HashMap<String, AppNote> {
    notes_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_notes(notes: &HashMap<String, AppNote>) -> Result<(), String> {
    let path = notes_path().ok_or("cannot resolve notes path")?;
    let json = serde_json::to_string_pretty(notes).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Record a note and/or ignore flag for an app, used in future AI checks.
/// An empty note with ignore=false removes the entry.
#[tauri::command]
pub async fn set_app_note(name: String, note: String, ignore: bool) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let mut notes = load_notes();
        let key = name.to_lowercase();
        if note.trim().is_empty() && !ignore {
            notes.remove(&key);
        } else {
            notes.insert(
                key,
                AppNote {
                    ignore,
                    note: note.trim().to_string(),
                },
            );
        }
        save_notes(&notes)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone, Debug)]
pub struct AppUpdate {
    pub id: String,
    pub name: String,
    pub current: String,
    pub available: String,
}

/// The columns a winget table can have, in display order.
const WINGET_COLUMNS: [&str; 5] = ["Name", "Id", "Version", "Available", "Source"];

/// Locate each column's start (char offset) from the table header. winget aligns
/// columns to fixed positions under their header labels, so this is far more
/// robust than splitting on whitespace: winget separates columns with a *single*
/// space (the wide gaps are just padding to the widest cell), and it truncates
/// long fields with '…'. Both break a space-run splitter but leave column
/// positions intact. The header is often prefixed with progress-spinner output
/// terminated by '\r', so only the text after the last '\r' is considered.
fn header_offsets(text: &str) -> Vec<(&'static str, usize)> {
    let header = text
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l))
        .find(|l| l.contains("Id") && l.contains("Version"));
    let mut offsets = Vec::new();
    if let Some(h) = header {
        for label in WINGET_COLUMNS {
            if let Some(byte) = h.find(label) {
                offsets.push((label, h[..byte].chars().count()));
            }
        }
        offsets.sort_by_key(|&(_, start)| start);
    }
    offsets
}

/// Read one column's trimmed value from a row. A column spans from its own start
/// to the next column's start (the last runs to end of line). Returns "" when the
/// column is absent or starts past the row's end.
fn column(offsets: &[(&'static str, usize)], row: &[char], label: &str) -> String {
    let Some(idx) = offsets.iter().position(|&(l, _)| l == label) else {
        return String::new();
    };
    let start = offsets[idx].1;
    if start >= row.len() {
        return String::new();
    }
    let end = offsets
        .get(idx + 1)
        .map(|&(_, s)| s.min(row.len()))
        .unwrap_or(row.len());
    row[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Split a winget table into its column offsets and data rows (as char vectors).
/// Skips the progress-noise and header above the dashed separator, and stops at
/// the "N upgrades available" footer (and the "explicit targeting" sub-table some
/// winget versions append).
fn winget_table(text: &str) -> (Vec<(&'static str, usize)>, Vec<Vec<char>>) {
    let offsets = header_offsets(text);
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            if trimmed.contains("-----") {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if lower.contains("upgrade") && lower.contains("available")
            || lower.starts_with("the following packages")
        {
            break;
        }
        rows.push(line.chars().collect());
    }
    (offsets, rows)
}

/// Parse `winget upgrade` output into the apps with an available update.
fn parse_upgrades(text: &str) -> Vec<AppUpdate> {
    let (offsets, rows) = winget_table(text);
    let mut updates = Vec::new();
    for row in &rows {
        let name = column(&offsets, row, "Name");
        // Strip the truncation ellipsis winget adds to long ids; winget's `--id`
        // does substring matching, so the un-truncated prefix still resolves.
        let id = column(&offsets, row, "Id")
            .trim_end_matches('…')
            .trim_end_matches('.')
            .to_string();
        let current = column(&offsets, row, "Version");
        let available = column(&offsets, row, "Available");
        if id.is_empty() || available.is_empty() {
            continue;
        }
        updates.push(AppUpdate {
            id,
            name,
            current,
            available,
        });
    }
    updates
}

/// List apps with an available update. Runs unelevated — listing needs no admin.
#[tauri::command]
pub async fn list_app_updates() -> Result<Vec<AppUpdate>, String> {
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("winget")
            .args([
                "upgrade",
                "--include-unknown",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_upgrades(&text))
}

/// Unique temp path for staging the elevated script / capturing its output.
/// pid + a process-lifetime counter keeps concurrent calls from colliding.
fn temp_file(ext: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("eir-winget-{}-{seq}.{ext}", std::process::id()))
}

/// Run an elevated winget command via UAC and capture winget's own output.
///
/// `Start-Process -Verb RunAs` elevates through ShellExecute, which cannot
/// redirect the child's stdio — so a bare exit code is all the old code could
/// surface. Instead the elevated winget writes all its streams to a temp log we
/// read back, so a failure carries winget's real message (e.g. the portable
/// "use --force" guard) rather than just a number. Returns (exit_code, output).
async fn run_winget_elevated_raw(args: Vec<String>) -> Result<(i32, String), String> {
    tokio::task::spawn_blocking(move || elevated_winget_blocking(args))
        .await
        .map_err(|e| e.to_string())?
}

fn elevated_winget_blocking(args: Vec<String>) -> Result<(i32, String), String> {
    let log = temp_file("log");
    let script = temp_file("ps1");

    // Single-quoted PowerShell list ('' escapes a quote); spread into winget via @a.
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let log_lit = log.to_string_lossy().replace('\'', "''");
    // Elevated body: merge every winget stream and write it UTF-8 to the log,
    // then propagate winget's own exit code. -Encoding utf8 avoids PS5's UTF-16
    // default so the Rust side reads it cleanly.
    let inner = format!(
        "$a=@({arg_list})\r\n\
         & winget @a *>&1 | Out-File -FilePath '{log_lit}' -Encoding utf8\r\n\
         exit $LASTEXITCODE\r\n"
    );
    std::fs::write(&script, inner).map_err(|e| format!("could not stage winget script: {e}"))?;

    let script_lit = script.to_string_lossy().replace('\'', "''");
    // Unelevated launcher: trigger the UAC prompt, wait, and pass the code back.
    // A declined prompt throws — report it as ERROR_CANCELLED (1223), not a crash.
    let outer = format!(
        "try {{ $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{script_lit}'; \
         exit $p.ExitCode }} catch {{ exit 1223 }}"
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &outer])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;

    let captured = std::fs::read(&log)
        .map(|b| {
            String::from_utf8_lossy(&b)
                .trim_start_matches('\u{feff}')
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(&script);

    Ok((status.code().unwrap_or(-1), captured))
}

/// The full `winget upgrade` argument list for a target (an `--id <id>` pair or
/// `--all`), optionally forcing past the portable-integrity check.
fn upgrade_args(target: &[String], force: bool) -> Vec<String> {
    let mut a = vec!["upgrade".to_string()];
    a.extend(target.iter().cloned());
    a.extend([
        "--silent".to_string(),
        "--accept-package-agreements".to_string(),
        "--accept-source-agreements".to_string(),
        "--disable-interactivity".to_string(),
    ]);
    if force {
        a.push("--force".to_string());
    }
    a
}

/// winget refused because a portable package's files changed after install — its
/// documented remedy is to re-run with --force. Self-updating CLIs (e.g. the
/// GitHub Copilot CLI, `GitHub.Copilot`) trip this on every upgrade.
fn portable_modified(output: &str) -> bool {
    let l = output.to_lowercase();
    l.contains("has been modified") && l.contains("--force")
}

/// Turn an (exit code, output) pair into a user-facing result, keeping winget's
/// own message on failure so the UI/logs can show *why* it failed.
fn winget_outcome(code: i32, output: &str) -> Result<String, String> {
    if code == 0 {
        return Ok(if output.is_empty() {
            "ok".to_string()
        } else {
            output.to_string()
        });
    }
    if code == 1223 {
        return Err("update cancelled at the UAC prompt".to_string());
    }
    let detail: String = output.chars().take(600).collect();
    if detail.is_empty() {
        Err(format!("winget exited with code {code}"))
    } else {
        Err(format!("winget failed (code {code}): {detail}"))
    }
}

/// Run an elevated `winget upgrade`, retrying once with --force if the package is
/// a portable whose files were modified after install (the only case --force is
/// auto-applied — a click on "Update" is unambiguous intent to take the version).
async fn run_winget_upgrade(target: Vec<String>) -> Result<String, String> {
    let (code, out) = run_winget_elevated_raw(upgrade_args(&target, false)).await?;
    if code != 0 && portable_modified(&out) {
        let (code, out) = run_winget_elevated_raw(upgrade_args(&target, true)).await?;
        return winget_outcome(code, &out);
    }
    winget_outcome(code, &out)
}

/// Update one winget app, then verify it now reports the new version. Returns a
/// structured outcome (success + verification + winget's own message) so the UI
/// can show a green/amber/red badge instead of an opaque string.
#[tauri::command]
pub async fn update_app(
    app: AppHandle,
    id: String,
    name: String,
    current: String,
    available: String,
) -> Result<AppOutcome, String> {
    emit_phase(&app, &id, "installing", 0, 1);
    let res = run_winget_upgrade(vec!["--id".to_string(), id.clone()]).await;
    let mut o = AppOutcome::new(&id, &name, "winget", &current);
    match res {
        Ok(detail) => {
            o.detail = detail;
            emit_phase(&app, &id, "verifying", 0, 1);
            let (verification, found) =
                verify_app(&VerifyTarget::Winget { id: id.clone() }, &available).await;
            o.success = verification != "mismatch";
            o.verification = verification;
            o.to = found;
        }
        Err(e) => {
            o.success = false;
            o.detail = e;
        }
    }
    emit_phase(&app, &id, if o.success { "done" } else { "failed" }, 1, 1);
    Ok(o)
}

/// Update every winget-managed app under one UAC prompt, then verify each one
/// individually so the UI shows per-app results rather than one opaque blob.
#[tauri::command]
pub async fn update_all_apps(app: AppHandle) -> Result<Vec<AppOutcome>, String> {
    let before = list_app_updates().await.unwrap_or_default();
    emit_phase(&app, "*", "installing", 0, before.len());
    let bulk = run_winget_upgrade(vec!["--all".to_string()]).await;
    Ok(verify_winget_batch(&app, &before, &bulk).await)
}

/// After a `winget upgrade --all`, re-query each previously-listed app's version
/// and build a per-app outcome. Verification is what decides success — a winget
/// "ok" that didn't actually change the version surfaces as a mismatch.
async fn verify_winget_batch(
    app: &AppHandle,
    before: &[AppUpdate],
    bulk: &Result<String, String>,
) -> Vec<AppOutcome> {
    let detail = match bulk {
        Ok(s) => s.clone(),
        Err(e) => e.clone(),
    };
    let total = before.len();
    let mut outcomes = Vec::with_capacity(total);
    for (i, up) in before.iter().enumerate() {
        emit_phase(app, &up.id, "verifying", i, total);
        let (verification, found) =
            verify_app(&VerifyTarget::Winget { id: up.id.clone() }, &up.available).await;
        let mut o = AppOutcome::new(&up.id, &up.name, "winget", &up.current);
        o.success = bulk.is_ok() && verification != "mismatch";
        o.verification = verification;
        o.to = found;
        o.detail = detail.clone();
        emit_phase(app, &up.id, if o.success { "done" } else { "failed" }, i + 1, total);
        outcomes.push(o);
    }
    outcomes
}

// ── Update outcomes + live progress ──────────────────────────────────────────

/// The result of trying to update one app. Flat strings/bools so the UI can
/// render a badge without decoding tagged enums.
#[derive(Serialize, Clone, Debug)]
pub struct AppOutcome {
    /// Stable id for the row: the winget id, else the lowercased name.
    pub key: String,
    pub name: String,
    /// "winget" | "ai" | "manual"
    pub method: String,
    pub from: String,
    /// Version observed AFTER the update (empty if not confirmed).
    pub to: String,
    pub success: bool,
    /// winget/installer message, or the reason it couldn't proceed.
    pub detail: String,
    /// "verified" | "mismatch" | "unverified" | "not_checked"
    pub verification: String,
    /// Authenticode result for AI installs: "", "signed: <CN>", "unsigned", "untrusted (..)".
    pub signature: String,
    pub cost_usd: f64,
    /// AI target version + URLs, so an "Update everything" row can offer Download
    /// / show the target without a fresh (paid) re-check. Empty for winget apps.
    #[serde(default)]
    pub latest: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub releases_url: String,
}

impl AppOutcome {
    fn new(key: &str, name: &str, method: &str, from: &str) -> Self {
        Self {
            key: key.to_string(),
            name: name.to_string(),
            method: method.to_string(),
            from: from.to_string(),
            to: String::new(),
            success: false,
            detail: String::new(),
            verification: "not_checked".to_string(),
            signature: String::new(),
            cost_usd: 0.0,
            latest: String::new(),
            url: String::new(),
            releases_url: String::new(),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
struct PhaseEvent {
    key: String,
    phase: String,
    index: usize,
    total: usize,
}

/// Push a live phase update to the UI (no polling): the row keyed by `key` shows
/// planning -> downloading -> installing -> verifying -> done/failed.
fn emit_phase(app: &AppHandle, key: &str, phase: &str, index: usize, total: usize) {
    let _ = app.emit(
        "update-progress",
        PhaseEvent {
            key: key.to_string(),
            phase: phase.to_string(),
            index,
            total,
        },
    );
}

// ── Post-update verification ─────────────────────────────────────────────────

enum VerifyTarget {
    Winget { id: String },
    ByName { name: String, verify_exe: Option<String> },
}

/// Confirm an app now reports `expected` (or newer). Returns (verification, found
/// version). One short retry absorbs the ARP-registration lag right after a fresh
/// install before declaring a version unverifiable.
async fn verify_app(target: &VerifyTarget, expected: &str) -> (String, String) {
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let (found, from_exe) = match target {
            VerifyTarget::Winget { id } => (winget_installed_version(id).await, false),
            VerifyTarget::ByName { name, verify_exe } => {
                let v = winget_installed_version_by_name(name).await;
                match (v, verify_exe) {
                    (Some(v), _) => (Some(v), false),
                    (None, Some(exe)) => (exe_file_version(exe).await, true),
                    (None, None) => (None, false),
                }
            }
        };
        if let Some(found) = found {
            let mut verdict = classify_version(&found, expected);
            // A binary's ProductVersion is often a 4-part FILEVERSION that trails
            // the marketing/release version; don't hard-fail an install on that —
            // soften an exe-fallback mismatch to "unverified".
            if from_exe && verdict == "mismatch" {
                verdict = "unverified".to_string();
            }
            return (verdict, found);
        }
    }
    ("unverified".to_string(), String::new())
}

/// Map a found-vs-expected version comparison to a verification verdict.
fn classify_version(found: &str, expected: &str) -> String {
    if found.trim().is_empty() || found.eq_ignore_ascii_case("unknown") {
        return "unverified".to_string();
    }
    match version_cmp(found, expected) {
        // Still older than what we expected to install => the update didn't take.
        Some(std::cmp::Ordering::Less) => "mismatch".to_string(),
        // Equal or newer (vendor may have shipped an even newer build) => success.
        Some(_) => "verified".to_string(),
        None => {
            if normalize_version(found) == normalize_version(expected) {
                "verified".to_string()
            } else {
                "unverified".to_string()
            }
        }
    }
}

fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches(['v', 'V']).trim().to_string()
}

/// Compare two version strings numerically, component by component. Returns None
/// when neither side begins with a parseable dotted-numeric version.
fn version_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parse(s: &str) -> Option<Vec<u64>> {
        let s = normalize_version(s);
        let head: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let parts: Vec<u64> = head
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if parts.is_empty() {
            None
        } else {
            Some(parts)
        }
    }
    let (mut x, mut y) = (parse(a)?, parse(b)?);
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    Some(x.cmp(&y))
}

/// True only when `latest` is a STRICTLY newer version than `current`. Numeric
/// comparison when both parse as dotted-numeric; otherwise a normalized string
/// inequality, so an identical version is never reported as an update. An empty
/// `latest` is never newer; an empty/unknown `current` lets any real `latest`
/// through (we'd rather show an unverifiable update than silently hide one).
fn is_newer(latest: &str, current: &str) -> bool {
    if latest.trim().is_empty() {
        return false;
    }
    match version_cmp(latest, current) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(_) => false,
        None => normalize_version(latest) != normalize_version(current),
    }
}

/// Resolve the installed version of an AI-returned app name against the apps we
/// actually queried (keyed lowercased name -> version). Display names are fuzzy —
/// the model may echo "Revision Tool" for a winget "Revision Tool version 2.9.0" —
/// so fall back to a contains-either-way match after an exact hit fails.
fn match_installed<'a>(
    installed: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a String> {
    let n = name.to_lowercase();
    if let Some(v) = installed.get(&n) {
        return Some(v);
    }
    installed
        .iter()
        .find(|(k, _)| !k.is_empty() && (k.contains(&n) || n.contains(k.as_str())))
        .map(|(_, v)| v)
}

/// Read an installed app's version from `winget list --id <id> --exact`.
async fn winget_installed_version(id: &str) -> Option<String> {
    let id = id.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("winget")
            .args([
                "list",
                "--id",
                &id,
                "--exact",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (offsets, rows) = winget_table(&text);
    rows.first()
        .map(|r| column(&offsets, r, "Version"))
        .filter(|v| !v.is_empty())
}

/// Read an installed app's version from `winget list --name <name>`, matching the
/// row whose Name overlaps the queried name (display names are fuzzy).
async fn winget_installed_version_by_name(name: &str) -> Option<String> {
    let q = name.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("winget")
            .args([
                "list",
                "--name",
                &q,
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (offsets, rows) = winget_table(&text);
    let lname = name.to_lowercase();
    for r in &rows {
        let n = column(&offsets, r, "Name").to_lowercase();
        if !n.is_empty() && (n.contains(&lname) || lname.contains(&n)) {
            let v = column(&offsets, r, "Version");
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Read FileVersion/ProductVersion of an absolute exe path (a second confirmation
/// signal for AI installs). Returns None for relative paths or missing files.
async fn exe_file_version(path: &str) -> Option<String> {
    if !Path::new(path).is_absolute() {
        return None;
    }
    let script = format!(
        "try {{ (Get-Item -LiteralPath '{}').VersionInfo.ProductVersion }} catch {{ '' }}",
        path.replace('\'', "''")
    );
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Re-verify a single row on demand (e.g. an amber "unverified" row).
#[tauri::command]
pub async fn verify_app_version(
    name: String,
    winget_id: Option<String>,
    expected: String,
    verify_exe: Option<String>,
) -> Result<AppOutcome, String> {
    let key = winget_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name.to_lowercase());
    let target = match winget_id.filter(|s| !s.is_empty()) {
        Some(id) => VerifyTarget::Winget { id },
        None => VerifyTarget::ByName {
            name: name.clone(),
            verify_exe,
        },
    };
    let (verification, found) = verify_app(&target, &expected).await;
    let mut o = AppOutcome::new(&key, &name, "winget", "");
    o.success = verification != "mismatch";
    o.verification = verification;
    o.to = found;
    Ok(o)
}

// ── AI-driven install: plan, validate, download, verify, run ──────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallerKind {
    Exe,
    Msi,
}

/// Untrusted AI output — never used directly; sanitised by validate_plan.
#[derive(Deserialize, Default)]
struct InstallPlanRaw {
    // The prompt tells the model to use `null` for fields it can't fill. serde
    // refuses `null` for a plain String (which crashed the whole parse), so these
    // string/array fields accept null|missing as empty via de_null_*.
    #[serde(default, deserialize_with = "de_null_string")]
    installer_url: String,
    #[serde(default, deserialize_with = "de_null_string")]
    releases_url: String,
    #[serde(default, deserialize_with = "de_null_string")]
    expected_version: String,
    #[serde(default, deserialize_with = "de_null_vec")]
    silent_args: Vec<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default, deserialize_with = "de_null_string")]
    publisher: String,
    #[serde(default)]
    verify_exe: Option<String>,
}

/// Deserialize a string that the AI may send as JSON `null` (or omit) — both map
/// to an empty string instead of failing the whole parse.
fn de_null_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Same null-tolerance for a string array (the AI sometimes sends silent_args: null).
fn de_null_vec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// A server-validated install plan — the only plan the install pipeline trusts.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallPlan {
    pub name: String,
    pub current: String,
    pub installer_url: String,
    pub host: String,
    pub releases_url: Option<String>,
    pub expected_version: String,
    pub kind: InstallerKind,
    pub silent_args: Vec<String>,
    pub sha256: Option<String>,
    pub expected_publisher: Option<String>,
    pub verify_exe: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PlanResult {
    pub plan: Option<InstallPlan>,
    pub releases_url: Option<String>,
    pub cost_usd: f64,
    pub reason: Option<String>,
}

/// Multi-tenant release hosts trusted to serve any vendor's installer. A specific
/// vendor's own domain is accepted separately via host_matches_name.
const TRUSTED_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com"];

/// Two-label public suffixes we recognise so the brand label is taken from the
/// right position (e.g. vendor.co.uk -> "vendor", not "co"). Not exhaustive — an
/// unrecognised multi-part TLD just falls back to manual download, which is safe.
const MULTI_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "com.au", "co.nz", "co.jp", "com.br", "co.in", "co.za", "com.tr",
];

/// The brand label of a host: the label immediately left of the public suffix
/// (e.g. download.krita.org -> "krita", app.vendor.co.uk -> "vendor").
fn brand_label(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 {
        return None;
    }
    let last2 = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let suffix_labels = if MULTI_SUFFIXES.contains(&last2.as_str()) { 2 } else { 1 };
    labels
        .len()
        .checked_sub(suffix_labels + 1)
        .map(|i| labels[i].to_string())
}

fn host_trusted(host: &str) -> bool {
    let h = host.to_lowercase();
    TRUSTED_HOSTS.contains(&h.as_str())
        || h.ends_with(".github.io")
        // GitHub serves release assets from various CDN subdomains it owns
        // (objects./release-assets.githubusercontent.com); trust the whole domain.
        || h == "githubusercontent.com"
        || h.ends_with(".githubusercontent.com")
}

/// Lowercased alphanumeric token of a string (for app-name/domain matching).
fn alnum_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Whether a vendor domain belongs to the app: its BRAND label must EXACTLY equal
/// the app-name token (whole name or its first brand word). Equality — not a
/// substring test — so lookalikes like obsidian-download.com, notionx.io, or
/// brave.evil.com are rejected; only obsidian.md / krita.org / mozilla.org match.
fn host_matches_name(host: &str, name: &str) -> bool {
    let Some(brand) = brand_label(host).map(|b| alnum_token(&b)) else {
        return false;
    };
    if brand.len() < 4 {
        return false;
    }
    let full = alnum_token(name);
    let first = name
        .split_whitespace()
        .next()
        .map(alnum_token)
        .unwrap_or_default();
    (full.len() >= 4 && brand == full) || (first.len() >= 4 && brand == first)
}

fn host_acceptable(host: &str, name: &str) -> bool {
    host_trusted(host) || host_matches_name(host, name)
}

/// Strict gate for the initial URL and every redirect hop / final URL: https,
/// no credentials, default port, not a raw IP, not punycode/IDN, and an
/// acceptable host. Returns Err(reason) so callers can surface why a hop failed.
fn url_acceptable(u: &url::Url, name: &str) -> Result<(), &'static str> {
    if u.scheme() != "https" {
        return Err("not https");
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("embeds credentials");
    }
    if u.port().is_some() {
        return Err("non-default port");
    }
    let host = u.host_str().ok_or("no host")?.to_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err("raw IP host");
    }
    if host.starts_with("xn--") || host.contains(".xn--") {
        return Err("punycode/IDN host");
    }
    if !host_acceptable(&host, name) {
        return Err("untrusted host");
    }
    Ok(())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Keep only known, safe silent-install switches; drop anything with shell
/// metacharacters or whitespace so nothing extra can reach the elevated script.
fn sanitise_args(kind: InstallerKind, raw: &[String]) -> Vec<String> {
    const ALLOW_EXE: &[&str] = &[
        "/S",
        "/silent",
        "/verysilent",
        "/quiet",
        "/q",
        "/norestart",
        "/passive",
        "/suppressmsgboxes",
    ];
    const ALLOW_MSI: &[&str] = &["/qn", "/quiet", "/norestart", "/passive", "REBOOT=ReallySuppress"];
    let allow: &[&str] = match kind {
        InstallerKind::Exe => ALLOW_EXE,
        InstallerKind::Msi => ALLOW_MSI,
    };
    let mut out: Vec<String> = Vec::new();
    for a in raw {
        let t = a.trim();
        if t.is_empty()
            || t.chars().any(|c| {
                matches!(
                    c,
                    ' ' | '\t' | '\'' | '"' | ';' | '&' | '|' | '>' | '<' | '$' | '`' | '\n' | '\r'
                )
            })
        {
            continue;
        }
        if allow.iter().any(|x| x.eq_ignore_ascii_case(t))
            && !out.iter().any(|o| o.eq_ignore_ascii_case(t))
        {
            out.push(t.to_string());
        }
    }
    out
}

/// Deterministically validate an AI-proposed plan. Pure (no I/O) and unit-tested:
/// the AI only proposes; Rust disposes. Rejection => the UI falls back to a manual
/// browser download.
fn validate_plan(raw: InstallPlanRaw, name: &str, current: &str) -> Result<InstallPlan, String> {
    let url_str = raw.installer_url.trim().to_string();
    if url_str.is_empty() || url_str.eq_ignore_ascii_case("null") {
        return Err("no direct installer URL".into());
    }
    let parsed = url::Url::parse(&url_str).map_err(|_| "installer URL is not valid".to_string())?;
    if parsed.scheme() != "https" {
        return Err("installer URL is not https".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("installer URL embeds credentials".into());
    }
    if parsed.port().is_some() {
        return Err("installer URL uses a non-default port".into());
    }
    let host = parsed.host_str().ok_or("installer URL has no host")?.to_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err("installer URL host is a raw IP".into());
    }
    if host.starts_with("xn--") || host.contains(".xn--") {
        return Err("installer URL host is punycode/IDN".into());
    }
    if !host_acceptable(&host, name) {
        return Err(format!(
            "host '{host}' is not a trusted release host or the app's vendor domain"
        ));
    }
    let path = parsed.path().to_lowercase();
    let kind = if path.ends_with(".msi") {
        InstallerKind::Msi
    } else if path.ends_with(".exe") {
        InstallerKind::Exe
    } else {
        return Err("installer URL does not end in .exe or .msi".into());
    };
    let sha256 = match raw.sha256.as_ref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty() && s != "null") {
        Some(s) if is_hex64(&s) => Some(s),
        Some(_) => return Err("provided sha256 is not 64 hex characters".into()),
        None => None,
    };
    let expected_version = raw.expected_version.trim().to_string();
    if expected_version.is_empty() || expected_version.eq_ignore_ascii_case("null") {
        return Err("plan has no expected version".into());
    }
    let releases_url = {
        let r = raw.releases_url.trim();
        if r.starts_with("https://") {
            Some(r.to_string())
        } else {
            None
        }
    };
    let expected_publisher = {
        let p = raw.publisher.trim();
        if p.is_empty() || p.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(p.to_string())
        }
    };
    let verify_exe = raw
        .verify_exe
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"));
    // An MSI must always run silently; if no usable switch survived, use msiexec's
    // standard quiet flags. An .exe with no known switch stays empty and is then
    // routed to manual install (running it hidden would hang).
    let mut silent_args = sanitise_args(kind, &raw.silent_args);
    if kind == InstallerKind::Msi && silent_args.is_empty() {
        silent_args = vec!["/qn".to_string(), "/norestart".to_string()];
    }
    Ok(InstallPlan {
        name: name.to_string(),
        current: current.to_string(),
        installer_url: url_str,
        host,
        releases_url,
        expected_version,
        kind,
        silent_args,
        sha256,
        expected_publisher,
        verify_exe,
    })
}

fn install_plan_prompt(name: &str, current: &str, note_line: &str) -> String {
    format!(
        "You resolve the OFFICIAL direct download for ONE Windows app so it can be installed \
unattended. Use web search. Use ONLY the vendor's official domain or its official GitHub releases. \
Respond with JSON only — no markdown, no prose.\n\n\
Return exactly this shape:\n\
{{\"installer_url\":\"<https URL ENDING in .exe or .msi — the actual installer FILE for 64-bit \
Windows, machine-wide; NOT a landing/download page; null if you cannot find a direct file with high \
confidence>\",\"releases_url\":\"<official https releases/download page, or null>\",\
\"expected_version\":\"<version this installer produces>\",\"installer_kind\":\"exe|msi\",\
\"silent_args\":[<documented silent switches: NSIS [\\\"/S\\\"]; Inno [\\\"/VERYSILENT\\\",\\\"/NORESTART\\\"]; \
MSI [\\\"/qn\\\",\\\"/norestart\\\"]>],\"sha256\":\"<vendor-published 64-hex hash, or null>\",\
\"publisher\":\"<expected Authenticode signing subject, e.g. 'Mozilla Corporation', or null>\",\
\"verify_exe\":\"<absolute path to an installed .exe whose version proves success, or null>\"}}\n\n\
Rules: if winget can manage this app, set installer_url=null. Never return a URL behind a login, ad \
redirect, or file-locker. If unsure of a DIRECT installer file, set installer_url=null and give \
releases_url only. Respect any [user note] and never contradict it.\n\n\
APP: {name} ({current}){note_line}"
    )
}

async fn run_ai_plan(prompt: String) -> Result<(InstallPlanRaw, f64), String> {
    let (content, cost) = run_ai_raw(prompt).await?;
    let json = extract_json(&content);
    let raw: InstallPlanRaw =
        serde_json::from_str(json).map_err(|e| format!("could not parse install plan: {e}"))?;
    Ok((raw, cost))
}

/// Ask the AI for an install plan for one app and validate it. Returns a plan
/// when it passes every deterministic check, else None + a manual releases URL.
async fn make_plan(
    name: &str,
    current: &str,
) -> (Option<InstallPlan>, Option<String>, f64, Option<String>) {
    let notes = load_notes();
    let key = name.to_lowercase();
    if notes.get(&key).map(|x| x.ignore).unwrap_or(false) {
        return (None, None, 0.0, Some(format!("{name} is on your ignore list.")));
    }
    let note_line = match notes.get(&key) {
        Some(x) if !x.note.is_empty() => format!(" [user note: {}]", x.note),
        _ => String::new(),
    };
    let (raw, cost) = match run_ai_plan(install_plan_prompt(name, current, &note_line)).await {
        Ok(v) => v,
        Err(e) => return (None, None, 0.0, Some(e)),
    };
    let releases_pre = {
        let r = raw.releases_url.trim();
        if r.starts_with("https://") {
            Some(r.to_string())
        } else {
            None
        }
    };
    match validate_plan(raw, name, current) {
        Ok(plan) => {
            let rel = plan.releases_url.clone().or(releases_pre);
            (Some(plan), rel, cost, None)
        }
        Err(e) => (None, releases_pre, cost, Some(e)),
    }
}

/// Plan-only command (no download, no execution) — lets the UI decide between an
/// "Install" action and the manual "Download" fallback.
#[tauri::command]
pub async fn plan_app_install(name: String, current: String) -> Result<PlanResult, String> {
    let (plan, releases_url, cost_usd, reason) = make_plan(&name, &current).await;
    Ok(PlanResult {
        plan,
        releases_url,
        cost_usd,
        reason,
    })
}

// ── Download + signature (unelevated) ────────────────────────────────────────

fn stage_root() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|b| PathBuf::from(b).join("Eir").join("stage"))
        .unwrap_or_else(|| std::env::temp_dir().join("eir-stage"))
}

fn stage_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    stage_root().join(format!("{}-{seq}", std::process::id()))
}

/// Delete any installer staging dirs left by a previous run (called at startup).
pub fn cleanup_stale_stage_dirs() {
    let _ = std::fs::remove_dir_all(stage_root());
}

struct Staged {
    dir: PathBuf,
    file: PathBuf,
    /// SHA-256 of the downloaded file; re-checked in the elevated context.
    sha256: String,
    /// Human-readable Authenticode result (recorded, not blocking).
    signature: String,
}

/// Largest installer we will download.
const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

/// Stream a download to `dest`, enforcing https on every redirect hop, an
/// acceptable host (initial and final), a non-HTML content type, and the size
/// cap. Hashes as it writes and returns the lowercase hex SHA-256.
async fn stream_download(url: &str, name: &str, dest: &Path) -> Result<String, String> {
    let name_owned = name.to_string();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        // Re-apply the FULL initial-URL gate (https, no creds/port, no raw IP, no
        // IDN, acceptable host) to every hop — not just scheme + host.
        match url_acceptable(attempt.url(), &name_owned) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(format!("blocked redirect ({reason})")),
        }
    });
    let client = reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    // Re-check the FINAL URL after redirects with the same strict gate.
    let final_url = resp.url().clone();
    if let Err(reason) = url_acceptable(&final_url, name) {
        return Err(format!("download landed on an unacceptable URL ({reason})"));
    }
    if let Some(ct) = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let ct = ct.to_lowercase();
        if ct.contains("text/html") || ct.contains("application/xhtml") {
            return Err(format!("download is a web page, not an installer ({ct})"));
        }
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_INSTALLER_BYTES {
            return Err(format!("installer is too large ({len} bytes)"));
        }
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("could not create staged file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        total += chunk.len() as u64;
        if total > MAX_INSTALLER_BYTES {
            return Err("installer exceeded the 256 MiB cap".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write failed: {e}"))?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Read the Authenticode status + signer of a staged file. Recorded for display;
/// it never blocks (the user opted to allow unsigned installers).
async fn authenticode(file: &Path, expected_publisher: Option<&str>) -> String {
    let p = file.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{p}'; $subj=''; \
         if ($s.SignerCertificate) {{ $subj = $s.SignerCertificate.Subject }}; \
         Write-Output ($s.Status.ToString() + '|' + $subj)"
    );
    let raw = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_default();

    let (status, subject) = raw
        .split_once('|')
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .unwrap_or((raw.clone(), String::new()));
    let cn = subject
        .split(',')
        .find_map(|p| p.trim().strip_prefix("CN="))
        .unwrap_or("")
        .to_string();

    let base = if status.eq_ignore_ascii_case("Valid") {
        if cn.is_empty() {
            "signed".to_string()
        } else {
            format!("signed: {cn}")
        }
    } else if status.eq_ignore_ascii_case("NotSigned") || status.is_empty() {
        "unsigned".to_string()
    } else {
        format!("untrusted ({status})")
    };
    match expected_publisher {
        Some(exp)
            if status.eq_ignore_ascii_case("Valid")
                && !exp.is_empty()
                && !subject.to_lowercase().contains(&exp.to_lowercase()) =>
        {
            format!("{base} (publisher mismatch)")
        }
        _ => base,
    }
}

/// Download the plan's installer, hash it (hard-fail on a provided-hash mismatch),
/// and record its signature. Runs entirely UNELEVATED.
async fn download_and_check(plan: &InstallPlan) -> Result<Staged, String> {
    let dir = stage_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create staging dir: {e}"))?;
    let file = dir.join(match plan.kind {
        InstallerKind::Msi => "installer.msi",
        InstallerKind::Exe => "installer.exe",
    });
    let sha = match stream_download(&plan.installer_url, &plan.name, &file).await {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    if let Some(expected) = &plan.sha256 {
        if &sha != expected {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!(
                "download SHA-256 mismatch (expected {expected}, got {sha}) — aborted"
            ));
        }
    }
    let signature = authenticode(&file, plan.expected_publisher.as_deref()).await;
    Ok(Staged {
        dir,
        file,
        sha256: sha,
        signature,
    })
}

// ── Elevated installer run ───────────────────────────────────────────────────

#[derive(Clone)]
struct InstallJob {
    key: String,
    kind: InstallerKind,
    file: PathBuf,
    args: Vec<String>,
    /// SHA-256 computed at download time; re-checked inside the elevated context
    /// right before launch so a same-user process cannot swap the staged file
    /// between the unelevated download and the elevated run (TOCTOU).
    sha256: String,
}

/// Run one or more already-downloaded installers in a SINGLE elevated PowerShell
/// session (one UAC prompt for the whole batch). Returns key -> exit code. Codes:
/// 0/3010 success, 1223 the whole UAC was declined, -2 timeout, -3 launch error.
async fn run_installs_elevated(jobs: Vec<InstallJob>) -> Result<HashMap<String, i32>, String> {
    tokio::task::spawn_blocking(move || installs_elevated_blocking(jobs))
        .await
        .map_err(|e| e.to_string())?
}

fn installs_elevated_blocking(jobs: Vec<InstallJob>) -> Result<HashMap<String, i32>, String> {
    let log = temp_file("log");
    let script = temp_file("ps1");
    let log_lit = log.to_string_lossy().replace('\'', "''");

    let mut body = String::new();
    for job in &jobs {
        let key = job.key.replace('\'', "''");
        let file = job.file.to_string_lossy().replace('\'', "''");
        let file_q = format!("'{file}'");
        let want = job.sha256.to_lowercase().replace('\'', "''");
        // Only the local file path + allow-listed args ever reach the shell.
        let (fp, mut arglist) = match job.kind {
            InstallerKind::Exe => (file_q.clone(), Vec::<String>::new()),
            InstallerKind::Msi => ("'msiexec'".to_string(), vec!["'/i'".to_string(), file_q.clone()]),
        };
        for a in &job.args {
            arglist.push(format!("'{}'", a.replace('\'', "''")));
        }
        let args_ps = if arglist.is_empty() {
            String::new()
        } else {
            format!(" -ArgumentList {}", arglist.join(","))
        };
        // Re-hash in the elevated context and refuse to run if the staged file
        // changed since download (-4). Then run with a 10-minute watchdog.
        body.push_str(&format!(
            "$code = -3\r\n\
             $want = '{want}'\r\n\
             $got = ''\r\n\
             try {{ $got = (Get-FileHash -LiteralPath {file_q} -Algorithm SHA256).Hash.ToLower() }} catch {{}}\r\n\
             if ($want -ne '' -and $got -ne $want) {{ $code = -4 }} else {{ \
             try {{ $p = Start-Process -FilePath {fp}{args_ps} -PassThru -WindowStyle Hidden -ErrorAction Stop; \
             if ($p.WaitForExit(600000)) {{ $code = $p.ExitCode }} else {{ try {{ $p.Kill() }} catch {{}}; $code = -2 }} }} \
             catch {{ $code = -3 }} }}\r\n\
             \"===EIR`t{key}`t$code===\" | Out-File -FilePath '{log_lit}' -Append -Encoding utf8\r\n"
        ));
    }
    body.push_str("exit 0\r\n");
    std::fs::write(&script, &body).map_err(|e| format!("could not stage install script: {e}"))?;

    let script_lit = script.to_string_lossy().replace('\'', "''");
    let outer = format!(
        "try {{ $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{script_lit}'; exit $p.ExitCode }} \
         catch {{ exit 1223 }}"
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &outer])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;

    let captured = std::fs::read(&log)
        .map(|b| {
            String::from_utf8_lossy(&b)
                .trim_start_matches('\u{feff}')
                .to_string()
        })
        .unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(&script);

    // A declined UAC prompt aborts the whole batch before any item ran.
    if status.code() == Some(1223) && captured.trim().is_empty() {
        return Err("cancelled at the UAC prompt".into());
    }

    let mut map = HashMap::new();
    for line in captured.lines() {
        if let Some(rest) = line.trim().strip_prefix("===EIR") {
            let rest = rest.trim_start_matches('\t').trim_end_matches("===");
            if let Some((k, c)) = rest.rsplit_once('\t') {
                if let Ok(code) = c.trim().parse::<i32>() {
                    map.insert(k.to_string(), code);
                }
            }
        }
    }
    Ok(map)
}

/// True when an installer exit code means success (0 = done, 3010 = needs reboot).
fn install_ok(code: i32) -> bool {
    code == 0 || code == 3010
}

/// Whether a validated plan can be installed unattended. An .exe with no known
/// silent switch is refused (running it hidden would hang) — manual fallback.
fn plan_runnable(plan: &InstallPlan) -> bool {
    !(plan.kind == InstallerKind::Exe && plan.silent_args.is_empty())
}

/// Download, install (one UAC), and verify a single validated plan.
async fn install_validated_plan(app: &AppHandle, plan: &InstallPlan, cost: f64) -> AppOutcome {
    let key = plan.name.to_lowercase();
    let mut o = AppOutcome::new(&key, &plan.name, "ai", &plan.current);
    o.cost_usd = cost;
    o.latest = plan.expected_version.clone();
    o.url = plan.installer_url.clone();
    o.releases_url = plan.releases_url.clone().unwrap_or_default();

    emit_phase(app, &key, "downloading", 0, 1);
    let staged = match download_and_check(plan).await {
        Ok(s) => s,
        Err(e) => {
            o.detail = e;
            emit_phase(app, &key, "failed", 1, 1);
            return o;
        }
    };
    o.signature = staged.signature.clone();

    emit_phase(app, &key, "installing", 0, 1);
    let job = InstallJob {
        key: key.clone(),
        kind: plan.kind,
        file: staged.file.clone(),
        args: plan.silent_args.clone(),
        sha256: staged.sha256.clone(),
    };
    let codes = run_installs_elevated(vec![job]).await;
    let _ = std::fs::remove_dir_all(&staged.dir);

    match codes {
        Err(e) => {
            o.detail = e;
            emit_phase(app, &key, "failed", 1, 1);
        }
        Ok(map) => {
            let code = map.get(&key).copied().unwrap_or(-1);
            apply_install_result(app, &mut o, plan, code).await;
        }
    }
    o
}

/// Given an installer exit code, verify the new version and finalise the outcome.
async fn apply_install_result(app: &AppHandle, o: &mut AppOutcome, plan: &InstallPlan, code: i32) {
    let key = o.key.clone();
    if install_ok(code) {
        emit_phase(app, &key, "verifying", 0, 1);
        let (verification, found) = verify_app(
            &VerifyTarget::ByName {
                name: plan.name.clone(),
                verify_exe: plan.verify_exe.clone(),
            },
            &plan.expected_version,
        )
        .await;
        o.success = verification != "mismatch";
        o.to = found;
        o.detail = if o.success {
            format!("installed{}", if code == 3010 { " (reboot required)" } else { "" })
        } else {
            "installer ran but the new version was not detected".into()
        };
        o.verification = verification;
    } else {
        o.detail = match code {
            1223 => "cancelled at the UAC prompt".into(),
            -4 => "staged installer changed before launch — aborted (possible tampering)".into(),
            -2 => "installer timed out and was stopped".into(),
            -3 => "installer could not be launched".into(),
            other => format!("installer exited with code {other}"),
        };
    }
    emit_phase(app, &key, if o.success { "done" } else { "failed" }, 1, 1);
}

/// Build a manual-fallback outcome (no direct installer, or no silent switch).
fn manual_outcome(name: &str, current: &str, cost: f64, detail: String, releases: Option<String>) -> AppOutcome {
    let mut o = AppOutcome::new(&name.to_lowercase(), name, "manual", current);
    o.cost_usd = cost;
    o.releases_url = releases.clone().unwrap_or_default();
    o.detail = match releases {
        Some(r) => format!("{detail} (releases: {r})"),
        None => detail,
    };
    o
}

/// A non-app status row for the "Update everything" summary (e.g. the AI check
/// failed, or only the first N apps were attempted). Carries no real row key.
fn info_outcome(detail: String, success: bool) -> AppOutcome {
    let mut o = AppOutcome::new("__info__", "Other-app check", "manual", "");
    o.success = success;
    o.detail = detail;
    o
}

/// Update one non-winget app via the AI: plan -> validate -> download -> install
/// -> verify. Falls back to a manual outcome when no safe direct install exists.
#[tauri::command]
pub async fn install_ai_app(app: AppHandle, name: String, current: String) -> Result<AppOutcome, String> {
    let key = name.to_lowercase();
    emit_phase(&app, &key, "planning", 0, 1);
    let (plan, releases, cost, reason) = make_plan(&name, &current).await;
    match plan {
        Some(plan) if plan_runnable(&plan) => Ok(install_validated_plan(&app, &plan, cost).await),
        Some(_) => {
            emit_phase(&app, &key, "failed", 1, 1);
            Ok(manual_outcome(
                &name,
                &current,
                cost,
                "no silent-install switch known — install manually".into(),
                releases,
            ))
        }
        None => {
            emit_phase(&app, &key, "failed", 1, 1);
            Ok(manual_outcome(
                &name,
                &current,
                cost,
                reason.unwrap_or_else(|| "no direct installer found — use Download".into()),
                releases,
            ))
        }
    }
}

/// Cap on AI-driven installs attempted per "Update everything" run.
const AI_INSTALL_CAP: usize = 10;

/// Update EVERYTHING: winget-managed apps in one batch (one UAC) + verify, then
/// the non-winget apps the AI can install — downloaded unelevated, then run in a
/// single elevated batch (one more UAC) and verified. Returns a per-app outcome
/// list; one app's failure never stops the rest.
#[tauri::command]
pub async fn update_everything(app: AppHandle) -> Result<Vec<AppOutcome>, String> {
    let mut outcomes: Vec<AppOutcome> = Vec::new();

    // Phase 1 — winget (trusted, batched).
    let winget = list_app_updates().await.unwrap_or_default();
    if !winget.is_empty() {
        emit_phase(&app, "*", "installing", 0, winget.len());
        let bulk = run_winget_upgrade(vec!["--all".to_string()]).await;
        outcomes.extend(verify_winget_batch(&app, &winget, &bulk).await);
    }

    // Phase 2 — AI installs. Surface check failures/truncation as visible rows
    // instead of collapsing them into a silent "nothing to update".
    let pending: Vec<AiUpdate> = match check_ai_updates().await {
        Ok(ai) => {
            if let Some(note) = ai.note.filter(|n| !n.is_empty()) {
                outcomes.push(info_outcome(note, true));
            }
            let ups = ai.updates;
            if ups.len() > AI_INSTALL_CAP {
                outcomes.push(info_outcome(
                    format!(
                        "{} other apps need updating; installing the first {}.",
                        ups.len(),
                        AI_INSTALL_CAP
                    ),
                    true,
                ));
            }
            ups.into_iter().take(AI_INSTALL_CAP).collect()
        }
        Err(e) => {
            outcomes.push(info_outcome(
                format!("couldn't check non-winget apps: {e}"),
                false,
            ));
            vec![]
        }
    };
    let total = pending.len();

    let mut jobs: Vec<InstallJob> = Vec::new();
    let mut staged_dirs: Vec<PathBuf> = Vec::new();
    // Keyed by a unique per-app id (the loop index) so two apps whose names fold
    // to the same string never collide; the display key lives on AppOutcome.key.
    let mut plans: HashMap<String, InstallPlan> = HashMap::new();
    let mut ai_outcomes: HashMap<String, AppOutcome> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for (i, up) in pending.iter().enumerate() {
        let id = i.to_string();
        let disp = up.name.to_lowercase();
        order.push(id.clone());
        emit_phase(&app, &disp, "planning", i, total);
        let (plan, releases, cost, reason) = make_plan(&up.name, &up.current).await;
        match plan {
            Some(plan) if plan_runnable(&plan) => {
                emit_phase(&app, &disp, "downloading", i, total);
                match download_and_check(&plan).await {
                    Ok(staged) => {
                        let mut o = AppOutcome::new(&disp, &up.name, "ai", &up.current);
                        o.cost_usd = cost;
                        o.signature = staged.signature.clone();
                        o.detail = "downloaded".into();
                        o.latest = plan.expected_version.clone();
                        o.url = plan.installer_url.clone();
                        o.releases_url = plan.releases_url.clone().unwrap_or_default();
                        staged_dirs.push(staged.dir.clone());
                        jobs.push(InstallJob {
                            key: id.clone(),
                            kind: plan.kind,
                            file: staged.file.clone(),
                            args: plan.silent_args.clone(),
                            sha256: staged.sha256.clone(),
                        });
                        plans.insert(id.clone(), plan);
                        ai_outcomes.insert(id, o);
                    }
                    Err(e) => {
                        let mut o = AppOutcome::new(&disp, &up.name, "ai", &up.current);
                        o.cost_usd = cost;
                        o.latest = up.latest.clone();
                        o.detail = e;
                        emit_phase(&app, &disp, "failed", i, total);
                        ai_outcomes.insert(id, o);
                    }
                }
            }
            other => {
                let detail = match other {
                    Some(_) => "no silent-install switch known — install manually".to_string(),
                    None => reason.unwrap_or_else(|| "no direct installer found".to_string()),
                };
                emit_phase(&app, &disp, "failed", i, total);
                let mut o = manual_outcome(&up.name, &up.current, cost, detail, releases);
                o.latest = up.latest.clone(); // show current → latest on the row
                ai_outcomes.insert(id, o);
            }
        }
    }

    // One elevated batch for every staged installer (one UAC for all of them).
    if !jobs.is_empty() {
        for j in &jobs {
            if let Some(o) = ai_outcomes.get(&j.key) {
                emit_phase(&app, &o.key, "installing", 0, jobs.len());
            }
        }
        match run_installs_elevated(jobs.clone()).await {
            Ok(map) => {
                for j in &jobs {
                    if let (Some(plan), Some(o)) =
                        (plans.get(&j.key), ai_outcomes.get_mut(&j.key))
                    {
                        let code = map.get(&j.key).copied().unwrap_or(-1);
                        apply_install_result(&app, o, plan, code).await;
                    }
                }
            }
            Err(e) => {
                for j in &jobs {
                    if let Some(o) = ai_outcomes.get_mut(&j.key) {
                        o.detail = e.clone();
                        let key = o.key.clone();
                        emit_phase(&app, &key, "failed", 1, 1);
                    }
                }
            }
        }
    }

    for d in &staged_dirs {
        let _ = std::fs::remove_dir_all(d);
    }
    for id in &order {
        if let Some(o) = ai_outcomes.remove(id) {
            outcomes.push(o);
        }
    }
    Ok(outcomes)
}

// ── AI update check (apps winget can't manage) ───────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct AiUpdate {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct AiCheckResult {
    pub updates: Vec<AiUpdate>,
    pub checked: usize,
    pub cost_usd: f64,
    pub note: Option<String>,
}

/// Cap on apps sent to the AI in one batch, to bound cost/latency.
const AI_CHECK_CAP: usize = 20;

/// Names we never AI-check: drivers, runtimes, redistributables, self-updating
/// suites, and Eir itself. Keeps the batch to real, user-updatable apps.
fn is_noise(name: &str) -> bool {
    let n = name.to_lowercase();
    const SKIP: &[&str] = &[
        "driver",
        "redistributable",
        "runtime",
        "microsoft visual c++",
        "windows sdk",
        "update for",
        "security update",
        "hotfix",
        "maintenance service",
        "microsoft .net",
        "directx",
        "realtek",
        "intel(r)",
        "host app",
        "web experience",
        "microsoft 365",
        "microsoft office",
        "visual studio installer",
        "onedrive",
        "teams machine-wide",
        "eir",
    ];
    SKIP.iter().any(|s| n.contains(s))
}

/// A winget *catalog* id looks like `Publisher.App` — a dot, no path separators
/// or spaces. winget can genuinely manage (and `winget upgrade` will flag) apps
/// with such ids, so they belong to Phase 1, not the AI check. A Microsoft Store
/// product id (e.g. `XPDC2RH70K22MN`) or an ARP id (`ARP\\Machine\\…`) is NOT a
/// catalog id — those are the correlated-standalone / unmanaged apps winget
/// upgrade silently ignores, which is exactly the gap the AI check must cover.
fn is_winget_catalog_id(id: &str) -> bool {
    let id = id.trim_end_matches('…');
    id.contains('.') && !id.contains('\\') && !id.contains('/') && !id.contains(' ')
}

/// Parse `winget list` for apps the AI should check for updates — the ones winget
/// upgrade cannot or will not handle.
///
/// The old code skipped any row whose Source was winget/msstore, assuming winget
/// would upgrade them. That silently hid standalone apps winget merely CORRELATED
/// to a catalog entry (e.g. Discord's per-user Squirrel install shows Store id
/// XPDC2RH70K22MN with Source=winget) but cannot actually upgrade. Instead we skip
/// only what winget genuinely owns or what updates elsewhere:
///   - `MSIX\` packages and msstore-source rows (true Store apps — update via the Store);
///   - winget-source rows with a real `Publisher.App` catalog id (winget upgrade owns these);
///   - noise (drivers/runtimes/etc.);
///   - anything Phase 1 already flagged (`already_managed`, case-insensitive by name).
///
/// Everything else — store-correlated standalone apps (Discord) and ARP/unmanaged
/// apps — is kept. Returns (name, version).
fn parse_unmanaged(text: &str, already_managed: &HashSet<String>) -> Vec<(String, String)> {
    let (offsets, rows) = winget_table(text);
    let mut apps = Vec::new();
    for row in &rows {
        let id = column(&offsets, row, "Id");
        if id.starts_with("MSIX\\") {
            continue;
        }
        let name = column(&offsets, row, "Name");
        let version = column(&offsets, row, "Version");
        if name.is_empty() || version.is_empty() || is_noise(&name) {
            continue;
        }
        // Source, ellipsis-stripped (winget truncates long cells with '…').
        let source = column(&offsets, row, "Source")
            .trim_end_matches('…')
            .to_lowercase();
        let is_msstore = source.len() >= 5 && "msstore".starts_with(source.as_str());
        let is_winget = source.len() >= 5 && "winget".starts_with(source.as_str());
        // True Store app, or an app winget genuinely manages -> not for the AI check.
        if is_msstore || (is_winget && is_winget_catalog_id(&id)) {
            continue;
        }
        if already_managed.contains(&name.to_lowercase()) {
            continue;
        }
        apps.push((name, version));
    }
    apps
}

fn claude_binary() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let candidate = format!("{profile}\\.local\\bin\\claude.exe");
        if std::path::Path::new(&candidate).is_file() {
            return candidate;
        }
    }
    "claude".into()
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    for (open, close) in [("```json", "```"), ("```", "```")] {
        if let Some(i) = t.find(open) {
            let after = &t[i + open.len()..];
            return after
                .find(close)
                .map(|e| &after[..e])
                .unwrap_or(after)
                .trim();
        }
    }
    t
}

#[derive(Deserialize)]
struct CliEnvelope {
    result: Option<String>,
    total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct AiResp {
    updates: Vec<AiUpdateRaw>,
}

#[derive(Deserialize)]
struct AiUpdateRaw {
    name: String,
    #[serde(default)]
    current: String,
    #[serde(default)]
    latest: String,
    #[serde(default)]
    url: String,
}

/// On-demand: ask the configured AI provider (with live web search) for updates
/// to apps winget can't manage. Uses OpenRouter's web plugin when the provider
/// is OpenRouter, otherwise the Claude CLI — see `run_ai_check`.
#[tauri::command]
pub async fn check_ai_updates() -> Result<AiCheckResult, String> {
    // 1. Apps winget can't manage.
    let list_out = tokio::task::spawn_blocking(|| {
        std::process::Command::new("winget")
            .args([
                "list",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    // Apps winget upgrade will already handle (Phase 1) — dedup them out by name.
    let managed: HashSet<String> = list_app_updates()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.name.to_lowercase())
        .collect();

    let notes = load_notes();
    let mut apps = parse_unmanaged(&String::from_utf8_lossy(&list_out.stdout), &managed);
    // Drop apps the user has chosen to ignore.
    apps.retain(|(n, _)| {
        !notes
            .get(&n.to_lowercase())
            .map(|x| x.ignore)
            .unwrap_or(false)
    });
    let total = apps.len();
    let mut note = None;
    if apps.len() > AI_CHECK_CAP {
        apps.truncate(AI_CHECK_CAP);
        note = Some(format!(
            "Checked the first {AI_CHECK_CAP} of {total} apps winget doesn't manage."
        ));
    }
    if apps.is_empty() {
        return Ok(AiCheckResult {
            updates: vec![],
            checked: 0,
            cost_usd: 0.0,
            note: Some("No non-winget apps to check.".into()),
        });
    }

    // 2. Ask Claude to look up the latest versions, honouring any user notes.
    let app_lines = apps
        .iter()
        .map(|(n, v)| match notes.get(&n.to_lowercase()) {
            Some(x) if !x.note.is_empty() => format!("- {n} ({v}) [user note: {}]", x.note),
            _ => format!("- {n} ({v})"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are an application update checker. Below are installed Windows applications with their \
current versions. Use web search to find each one's latest STABLE release from its official source. \
Return ONLY the apps that have a NEWER version available.\n\n\
Respect any [user note]: it may say an app is custom/self-built or give its real release source — \
follow that guidance and do NOT report an update that contradicts the note.\n\n\
Respond ONLY with JSON, no markdown:\n\
{{\"updates\":[{{\"name\":\"<app>\",\"current\":\"<installed>\",\"latest\":\"<newer version>\",\"url\":\"<official download or releases page URL>\"}}]}}\n\
Omit apps that are already current or that you cannot verify. Only include real, verified versions.\n\n\
INSTALLED APPS:\n{app_lines}"
    );

    let (raw_updates, cost) = run_ai_check(prompt).await?;
    // Deterministic guard: keep only updates that are STRICTLY newer than the
    // version actually installed right now. The model is told to return only newer
    // releases, but a fuzzy comparison (or an over-eager search) can slip an
    // equal/older "update" through — drop those, and stamp each surviving row with
    // the authoritative installed version so the UI never carries a stale `current`.
    let installed: HashMap<String, String> =
        apps.iter().map(|(n, v)| (n.to_lowercase(), v.clone())).collect();
    let updates = raw_updates
        .into_iter()
        .filter_map(|mut u| {
            let cur = match_installed(&installed, &u.name)
                .cloned()
                .unwrap_or_else(|| u.current.clone());
            if is_newer(&u.latest, &cur) {
                u.current = cur;
                Some(u)
            } else {
                None
            }
        })
        .collect();
    Ok(AiCheckResult {
        updates,
        checked: apps.len(),
        cost_usd: cost,
        note,
    })
}

/// Re-check a single app (using its stored note) — cheaper than a full sweep.
#[tauri::command]
pub async fn check_app_update(name: String, current: String) -> Result<AiCheckResult, String> {
    let notes = load_notes();
    let key = name.to_lowercase();
    if notes.get(&key).map(|x| x.ignore).unwrap_or(false) {
        return Ok(AiCheckResult {
            updates: vec![],
            checked: 1,
            cost_usd: 0.0,
            note: Some(format!("{name} is on your ignore list.")),
        });
    }
    // Re-read the version installed RIGHT NOW instead of trusting the value the UI
    // row captured when it was first rendered. After a manual upgrade that value is
    // stale, so the old code re-confirmed the same false positive every time — a
    // Re-check could never clear once the real install moved on. winget's live
    // reading wins; the caller's value is only a fallback when winget can't find it.
    let installed = winget_installed_version_by_name(&name)
        .await
        .unwrap_or(current);
    let note_line = match notes.get(&key) {
        Some(x) if !x.note.is_empty() => format!(" [user note: {}]", x.note),
        _ => String::new(),
    };
    let prompt = format!(
        "You are an application update checker. Use web search to find the latest STABLE release of \
the Windows app below from its official source, and report whether a newer version exists.\n\n\
Respect any [user note]: it may say the app is custom/self-built or give its real release source — \
follow it and do NOT report an update that contradicts the note.\n\n\
Respond ONLY with JSON, no markdown:\n\
{{\"updates\":[{{\"name\":\"{name}\",\"current\":\"<installed>\",\"latest\":\"<newer version>\",\"url\":\"<official download or releases page URL>\"}}]}}\n\
Return an empty updates array if it is already current or you cannot verify a newer version.\n\n\
APP: {name} ({installed}){note_line}"
    );
    let (updates, cost) = run_ai_check(prompt).await?;
    // Same strictly-newer guard as the full sweep: never surface an "update" that
    // isn't actually ahead of what's installed, and report the real current version.
    let updates = updates
        .into_iter()
        .filter(|u| is_newer(&u.latest, &installed))
        .map(|mut u| {
            u.current = installed.clone();
            u
        })
        .collect();
    Ok(AiCheckResult {
        updates,
        checked: 1,
        cost_usd: cost,
        note: None,
    })
}

/// Route an update-check prompt through whichever provider the service is
/// configured for (read from config.toml next to the executable):
/// OpenRouter free model + web plugin, or the Claude CLI (default fallback).
async fn run_ai_check(prompt: String) -> Result<(Vec<AiUpdate>, f64), String> {
    let (content, cost) = run_ai_raw(prompt).await?;
    let json = extract_json(&content);
    let resp: AiResp =
        serde_json::from_str(json).map_err(|e| format!("could not parse update list: {e}"))?;
    let updates = resp
        .updates
        .into_iter()
        .filter(|u| !u.name.is_empty() && !u.latest.is_empty())
        .map(|u| AiUpdate {
            name: u.name,
            current: u.current,
            latest: u.latest,
            url: u.url,
        })
        .collect();
    Ok((updates, cost))
}

/// Route a prompt to the configured provider and return the raw model text plus
/// the call's USD cost. Shared by the update check (run_ai_check) and the
/// install-plan resolution (run_ai_plan) so both honour the same provider config.
async fn run_ai_raw(prompt: String) -> Result<(String, f64), String> {
    let cfg = resolve_ai_cfg();
    if cfg.provider == "openrouter" {
        let key = cfg.openrouter_api_key.unwrap_or_default();
        if key.trim().is_empty() {
            return Err(
                "OpenRouter is selected but no API key is set — add it in Settings.".into(),
            );
        }
        let model = if cfg.model.trim().is_empty() {
            "openrouter/free".to_string()
        } else {
            cfg.model.trim().to_string()
        };
        run_openrouter_raw(prompt, &model, key.trim()).await
    } else {
        // Claude CLI (also the fallback for anthropic / openai_compatible, which
        // don't have a built-in web path here). update_check_model -> haiku.
        run_claude_raw(prompt, &cfg.update_check_model).await
    }
}

/// The AI settings the update check needs, read from the on-disk config.toml
/// (the service keeps the API key out of the pipe payload, so read it directly).
#[derive(Deserialize, Default)]
struct FileApiCfg {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    openrouter_api_key: Option<String>,
    #[serde(default)]
    update_check_model: String,
}

#[derive(Deserialize, Default)]
struct FileCfg {
    #[serde(default)]
    api: FileApiCfg,
}

fn resolve_ai_cfg() -> FileApiCfg {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.toml")))
        .unwrap_or_else(|| std::path::PathBuf::from("config.toml"));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<FileCfg>(&s).ok())
        .map(|c| c.api)
        .unwrap_or_default()
}

/// Ask OpenRouter (with its web-search plugin) for app updates. Works with free
/// models — OpenRouter performs the search and feeds results to the model, so it
/// is model-agnostic. Returns the parsed updates plus the call's USD cost.
async fn run_openrouter_raw(prompt: String, model: &str, key: &str) -> Result<(String, f64), String> {
    let body = serde_json::json!({
        "model": model,
        "plugins": [{ "id": "web", "max_results": 5 }],
        "messages": [{ "role": "user", "content": prompt }],
    });
    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("HTTP-Referer", "https://github.com/Swatto86/eir")
        .header("X-Title", "Eir")
        .json(&body)
        .timeout(std::time::Duration::from_secs(420))
        .send()
        .await
        .map_err(|e| format!("OpenRouter request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail: String = text.chars().take(400).collect();
        return Err(format!("OpenRouter error ({status}): {detail}"));
    }

    let parsed: OrResp =
        serde_json::from_str(&text).map_err(|e| format!("bad OpenRouter response: {e}"))?;
    if let Some(err) = parsed.error {
        return Err(format!("OpenRouter error: {}", err.message));
    }
    let content = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    let cost = parsed.usage.and_then(|u| u.cost).unwrap_or(0.0);
    Ok((content, cost))
}

#[derive(Deserialize)]
struct OrResp {
    #[serde(default)]
    choices: Vec<OrChoice>,
    #[serde(default)]
    usage: Option<OrUsage>,
    #[serde(default)]
    error: Option<OrError>,
}

#[derive(Deserialize)]
struct OrChoice {
    message: OrMsg,
}

#[derive(Deserialize)]
struct OrMsg {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OrUsage {
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct OrError {
    #[serde(default)]
    message: String,
}

/// Pull the JSON object out of a model response that may wrap it in prose or
/// code fences (reasoning models often add commentary around the JSON).
fn extract_json(s: &str) -> &str {
    let t = strip_fences(s);
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        if b > a {
            return &t[a..=b];
        }
    }
    t
}

/// Spawn `claude --print --output-format json` with the given prompt and model,
/// parse the result, and return the found updates plus the call's USD cost.
/// Map a requested model to a Claude model the CLI will accept. Claude aliases
/// (`haiku`/`sonnet`/`opus`) and any `claude*` id pass through; everything else
/// — blank, or a non-Claude id such as an OpenRouter model — becomes `haiku`.
fn claude_model_or_haiku(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_lowercase();
    let is_claude =
        matches!(lower.as_str(), "haiku" | "sonnet" | "opus") || lower.starts_with("claude");
    if is_claude {
        m.to_string()
    } else {
        "haiku".to_string()
    }
}

async fn run_claude_raw(prompt: String, model: &str) -> Result<(String, f64), String> {
    let binary = claude_binary();
    let mut std_cmd = std::process::Command::new(&binary);
    std_cmd.args(["--print", "--output-format", "json"]);
    // The app-update check needs live web search, so it must run on a Claude
    // model. Blank — or anything that isn't a Claude model (e.g. an OpenRouter
    // id left over from the decision-loop model) — falls back to Haiku, the
    // cheapest Claude model, so the check can never be handed a model the CLI
    // would reject.
    let model = claude_model_or_haiku(model);
    std_cmd.args(["--model", model.as_str()]);
    std_cmd.creation_flags(CREATE_NO_WINDOW);
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(420),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "AI check timed out after 7 minutes".to_string())?
    .map_err(|e| e.to_string())?;

    if !output.status.success() {
        // `claude --output-format json` writes its error to stdout, not stderr,
        // so check both. Prefer whichever has content.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let raw = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        let detail: String = if raw.is_empty() {
            "no output from claude — check the logged-in claude session".to_string()
        } else {
            raw.chars().take(400).collect()
        };
        return Err(format!(
            "claude exited (code {}) using model '{model}': {detail}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let env: CliEnvelope =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("bad claude output: {e}"))?;
    let cost = env.total_cost_usd.unwrap_or(0.0);
    Ok((env.result.unwrap_or_default(), cost))
}

/// Current USD→GBP rate for displaying costs in pounds. Fetched (cached by the
/// caller) with a sensible fallback if offline.
#[tauri::command]
pub async fn gbp_per_usd() -> Result<f64, String> {
    let rate = tokio::task::spawn_blocking(|| {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "try { (Invoke-RestMethod -Uri 'https://open.er-api.com/v6/latest/USD' -TimeoutSec 8).rates.GBP } catch { '' }",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        out.ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|r| *r > 0.1 && *r < 5.0)
            .unwrap_or(0.79)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(rate)
}

/// Open an http(s) URL in the user's default browser.
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http URL".into());
    }
    let script = format!("Start-Process '{}'", url.replace('\'', "''"));
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a winget-style fixed-width table from explicit column widths, a
    /// header, and data rows. Mirrors winget exactly: columns are left-aligned and
    /// joined by a single space (so when a field fills its column the gap is one
    /// space — the layout that defeats whitespace splitting), and any field longer
    /// than its column is truncated with a trailing '…'.
    fn render(widths: &[usize], header: &[&str], rows: &[&[&str]]) -> String {
        fn line(widths: &[usize], fields: &[&str]) -> String {
            let cells: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(c, &f)| {
                    let w = widths[c];
                    if f.chars().count() > w {
                        f.chars().take(w - 1).collect::<String>() + "…"
                    } else {
                        format!("{f:<w$}")
                    }
                })
                .collect();
            cells.join(" ").trim_end().to_string()
        }
        let total: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1);
        let mut out = line(widths, header);
        out.push('\n');
        out.push_str(&"-".repeat(total));
        for r in rows {
            out.push('\n');
            out.push_str(&line(widths, r));
        }
        out
    }

    #[test]
    fn header_offsets_ignore_progress_spinner_noise() {
        // winget prefixes the header with carriage-return-overwritten spinner text;
        // only the text after the last '\r' is the real header.
        let noisy = "  -  \r  \\  \rName    Id      Version  Source";
        let clean = "Name    Id      Version  Source";
        assert_eq!(header_offsets(noisy), header_offsets(clean));
        assert_eq!(header_offsets(clean).first(), Some(&("Name", 0)));
    }

    #[test]
    fn parse_upgrades_handles_single_space_columns() {
        // Every field fills its column, so winget separates them with a single
        // space — the narrow layout that previously parsed to zero upgrades.
        let widths = [11, 14, 7, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &[
                    "Copilot CLI",
                    "GitHub.Copilot",
                    "v1.0.44",
                    "v1.0.63",
                    "winget",
                ],
                &["7-Zip", "7zip.7zip", "25.01", "26.01", "winget"],
            ],
        );
        let table = format!("{table}\n2 upgrades available.");
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].name, "Copilot CLI");
        assert_eq!(ups[0].id, "GitHub.Copilot");
        assert_eq!(ups[0].current, "v1.0.44");
        assert_eq!(ups[0].available, "v1.0.63");
    }

    #[test]
    fn parse_upgrades_strips_truncated_id() {
        // A long id is truncated with '…'; winget's `--id` substring match still
        // resolves it, so we keep the prefix and drop the ellipsis.
        let widths = [30, 33, 23, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[&[
                "Visual Studio Build Tools 2022",
                "Microsoft.VisualStudio.2022.BuildTools",
                "17.14.25 (January 2026)",
                "17.14.34",
                "winget",
            ]],
        );
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].name, "Visual Studio Build Tools 2022");
        assert_eq!(ups[0].id, "Microsoft.VisualStudio.2022.Buil");
        assert_eq!(ups[0].current, "17.14.25 (January 2026)");
        assert_eq!(ups[0].available, "17.14.34");
    }

    #[test]
    fn unmanaged_keeps_correlated_standalone_and_dedupes_managed_and_store() {
        let widths = [24, 32, 18, 7];
        let header = ["Name", "Id", "Version", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &["7-Zip", "7zip.7zip", "25.01", "winget"], // already in winget upgrade → skip
                &["Discord", "XPDC2RH70K22MN", "1.0.9242", "winget"], // correlated standalone → KEEP
                &["iCloud", "9PKTQ5699M62", "15.8.127.0", "msstore"], // real Store/Appx → skip via token
                &["Git", "ARP\\Machine\\X64\\Git_is1", "2.52.0", ""], // unmanaged → keep
                &["Battle.net", "ARP\\Machine\\X86\\Battle.net", "Unknown", ""], // ".net" must not filter it
                &[
                    "NVIDIA Graphics Driver",
                    "ARP\\Machine\\X64\\{B2FE1952-0186-46C3}",
                    "596.49",
                    "",
                ], // driver → noise
                &[
                    "AV1 Video Extension",
                    "MSIX\\Microsoft.AV1VideoExtension_2.0.7.0_x64",
                    "2.0.7.0",
                    "",
                ], // MSIX/Store → skip
                &[
                    "Microsoft .NET Runtime",
                    "ARP\\Machine\\X64\\{DOTNET8}",
                    "8.0.11",
                    "",
                ], // runtime → noise
            ],
        );
        // Nothing pre-flagged by Phase 1; 7-Zip is excluded purely by its catalog id.
        let managed = std::collections::HashSet::new();
        let apps = parse_unmanaged(&table, &managed);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        // The regression guard: a winget-CORRELATED standalone app (Discord shows a
        // Store id + winget source, NOT a Publisher.App catalog id) is detected now.
        assert!(names.contains(&"Discord"), "Discord must be detected now");
        assert_eq!(
            apps.iter().find(|(n, _)| n == "Discord").unwrap().1,
            "1.0.9242"
        );
        assert!(names.contains(&"Git"));
        assert!(names.contains(&"Battle.net"));
        // 7-Zip excluded via its winget catalog id (winget manages it); iCloud via
        // msstore source; driver/runtime via noise; AV1 via the MSIX id.
        assert!(!names.contains(&"7-Zip"));
        assert!(!names.contains(&"iCloud"));
        assert!(!names.contains(&"NVIDIA Graphics Driver"));
        assert!(!names.contains(&"AV1 Video Extension"));
        assert!(!names.contains(&"Microsoft .NET Runtime"));
    }

    #[test]
    fn catalog_id_distinguishes_managed_from_correlated() {
        assert!(is_winget_catalog_id("Anthropic.Claude"));
        assert!(is_winget_catalog_id("7zip.7zip"));
        assert!(is_winget_catalog_id("JanDeDobbeleer.OhMyPosh"));
        assert!(is_winget_catalog_id("Microsoft.VisualStudio.2022.Buil…")); // truncated, still catalog
        assert!(!is_winget_catalog_id("XPDC2RH70K22MN")); // Store id (Discord)
        assert!(!is_winget_catalog_id("9PKTQ5699M62")); // Store id
        assert!(!is_winget_catalog_id("ARP\\Machine\\X64\\Git_is1")); // ARP id
    }

    #[test]
    fn unmanaged_dedupes_phase1_apps_case_insensitively() {
        let widths = [22, 26, 12, 7];
        let header = ["Name", "Id", "Version", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &["Obsidian", "ARP\\X64\\Obsidian", "1.5.0", "winget"],
                &["Krita", "ARP\\X64\\Krita", "5.2.0", ""],
            ],
        );
        // managed uses a different letter case than the display name.
        let managed: std::collections::HashSet<String> =
            ["obsidian".to_string()].into_iter().collect();
        let apps = parse_unmanaged(&table, &managed);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"Obsidian"));
        assert!(names.contains(&"Krita"));
    }

    #[test]
    fn portable_modified_detects_the_force_guard() {
        // The exact line winget prints for a self-updating portable (Copilot CLI).
        let out = "Starting package install...\n\
                   Unable to remove Portable package as it has been modified; \
                   to override this check use --force";
        assert!(portable_modified(out));
        // Unrelated failures must not trigger the --force retry.
        assert!(!portable_modified("Installer failed with exit code: 1603"));
        assert!(!portable_modified("No applicable upgrade found."));
    }

    #[test]
    fn upgrade_args_appends_force_only_when_asked() {
        let target = vec!["--id".to_string(), "GitHub.Copilot".to_string()];
        let plain = upgrade_args(&target, false);
        assert_eq!(plain.first().map(String::as_str), Some("upgrade"));
        assert!(plain.iter().any(|a| a == "GitHub.Copilot"));
        assert!(!plain.iter().any(|a| a == "--force"));
        assert!(upgrade_args(&target, true).iter().any(|a| a == "--force"));
    }

    #[test]
    fn winget_outcome_maps_codes_to_messages() {
        // Success returns winget's text (or "ok" when it printed nothing).
        assert_eq!(winget_outcome(0, "").unwrap(), "ok");
        assert_eq!(
            winget_outcome(0, "Successfully installed").unwrap(),
            "Successfully installed"
        );
        // A declined UAC prompt is reported as a cancellation, not a raw code.
        assert_eq!(
            winget_outcome(1223, "").unwrap_err(),
            "update cancelled at the UAC prompt"
        );
        // Other failures keep winget's own message so the UI can show why.
        let err = winget_outcome(-1, "0x8a150057 : the package is pinned").unwrap_err();
        assert!(err.contains("the package is pinned"));
    }

    #[test]
    fn non_claude_model_falls_back_to_haiku() {
        // Blank and non-Claude ids become haiku; Claude aliases/ids pass through.
        assert_eq!(claude_model_or_haiku(""), "haiku");
        assert_eq!(claude_model_or_haiku("  "), "haiku");
        assert_eq!(claude_model_or_haiku("nex-agi/nex-n2-pro:free"), "haiku");
        assert_eq!(
            claude_model_or_haiku("nvidia/nemotron-3-super-120b-a12b:free"),
            "haiku"
        );
        assert_eq!(claude_model_or_haiku("haiku"), "haiku");
        assert_eq!(claude_model_or_haiku("Sonnet"), "Sonnet");
        assert_eq!(claude_model_or_haiku("opus"), "opus");
        assert_eq!(
            claude_model_or_haiku("claude-haiku-4-5"),
            "claude-haiku-4-5"
        );
    }

    fn raw(url: &str) -> InstallPlanRaw {
        InstallPlanRaw {
            installer_url: url.to_string(),
            releases_url: String::new(),
            expected_version: "2.0.0".to_string(),
            silent_args: vec!["/S".to_string()],
            sha256: None,
            publisher: String::new(),
            verify_exe: None,
        }
    }

    #[test]
    fn validate_plan_accepts_github_release_exe() {
        let p = validate_plan(
            raw("https://github.com/foo/bar/releases/download/v2/Bar-setup.exe"),
            "Bar App",
            "1.0.0",
        )
        .unwrap();
        assert_eq!(p.kind, InstallerKind::Exe);
        assert_eq!(p.host, "github.com");
        assert_eq!(p.silent_args, vec!["/S".to_string()]);
    }

    #[test]
    fn install_plan_raw_tolerates_null_fields() {
        // The model is told to use null for fields it can't fill; null on a String
        // field previously crashed the whole parse ("invalid type: null") and forced
        // a manual fallback (the AllTheThings symptom).
        let json = r#"{"installer_url":null,"releases_url":"https://github.com/me/AllTheThings/releases","expected_version":"1.2.3","silent_args":null,"sha256":null,"publisher":null,"verify_exe":null}"#;
        let r: InstallPlanRaw = serde_json::from_str(json).expect("null fields must parse");
        assert_eq!(r.installer_url, "");
        assert!(r.silent_args.is_empty());
        assert_eq!(r.publisher, "");
        // No direct URL -> clean manual routing, not a parse crash.
        assert!(validate_plan(r, "AllTheThings", "1.0.0").is_err());
    }

    #[test]
    fn install_plan_raw_null_publisher_still_validates_github() {
        // A user's own GitHub tool: a direct .exe with null publisher/sha must
        // validate and be installable, not fall back to manual.
        let json = r#"{"installer_url":"https://github.com/me/AllTheThings/releases/download/v1.2.3/AllTheThings-setup.exe","releases_url":null,"expected_version":"1.2.3","silent_args":["/S"],"sha256":null,"publisher":null,"verify_exe":null}"#;
        let r: InstallPlanRaw = serde_json::from_str(json).unwrap();
        let p = validate_plan(r, "AllTheThings", "1.0.0").expect("github exe should validate");
        assert_eq!(p.host, "github.com");
        assert_eq!(p.expected_publisher, None);
        assert_eq!(p.silent_args, vec!["/S".to_string()]);
    }

    #[test]
    fn validate_plan_accepts_vendor_domain_and_defaults_msi_silent() {
        // Vendor domain accepted via the app-name token; MSI with no usable switch
        // falls back to msiexec's quiet flags so it never runs interactively.
        let p = validate_plan(
            raw("https://download.krita.org/installer/krita-x64.msi"),
            "Krita",
            "1.0",
        )
        .unwrap();
        assert_eq!(p.kind, InstallerKind::Msi);
        assert_eq!(p.silent_args, vec!["/qn".to_string(), "/norestart".to_string()]);
    }

    #[test]
    fn validate_plan_rejects_unsafe_urls() {
        assert!(validate_plan(raw("http://github.com/a/b/x.exe"), "X App", "1").is_err()); // not https
        assert!(validate_plan(raw("https://1.2.3.4/x.exe"), "X App", "1").is_err()); // raw IP
        assert!(validate_plan(raw("https://github.com/a/b/x.zip"), "X App", "1").is_err()); // bad extension
        assert!(validate_plan(raw("https://totally-unrelated.example/x.exe"), "Bar App", "1").is_err()); // untrusted host
        assert!(validate_plan(raw("https://user:pw@github.com/a/x.exe"), "X App", "1").is_err()); // credentials
    }

    #[test]
    fn validate_plan_rejects_bad_sha() {
        let mut r = raw("https://github.com/a/b/x.exe");
        r.sha256 = Some("not-hex".to_string());
        assert!(validate_plan(r, "X App", "1").is_err());
        let mut ok = raw("https://github.com/a/b/x.exe");
        ok.sha256 = Some("A".repeat(64));
        assert_eq!(
            validate_plan(ok, "X App", "1").unwrap().sha256,
            Some("a".repeat(64))
        );
    }

    #[test]
    fn sanitise_args_allow_lists_and_blocks_injection() {
        let exe = sanitise_args(
            InstallerKind::Exe,
            &[
                "/S".into(),
                "/VERYSILENT".into(),
                "; rm -rf".into(),
                "/x && calc".into(),
                "/norestart".into(),
            ],
        );
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/s")));
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/verysilent")));
        assert!(exe.iter().any(|a| a.eq_ignore_ascii_case("/norestart")));
        assert!(!exe
            .iter()
            .any(|a| a.contains("rm") || a.contains("calc") || a.contains('&')));
        // The MSI allow-list is separate; an exe-only switch is dropped.
        assert_eq!(sanitise_args(InstallerKind::Msi, &["/qn".into(), "/S".into()]), vec!["/qn".to_string()]);
    }

    #[test]
    fn host_gate_trusts_github_and_vendor_only() {
        assert!(host_acceptable("github.com", "Anything"));
        assert!(host_acceptable("objects.githubusercontent.com", "Anything"));
        assert!(host_acceptable("foo.github.io", "Anything"));
        // Exact brand-label match accepts the real vendor domain…
        assert!(host_acceptable("download.krita.org", "Krita"));
        assert!(host_acceptable("obsidian.md", "Obsidian"));
        assert!(host_acceptable("mozilla.org", "Mozilla Firefox"));
        // …but substring lookalikes and brand-as-subdomain tricks are REJECTED.
        assert!(!host_acceptable("obsidian-download.com", "Obsidian"));
        assert!(!host_acceptable("notionx.io", "Notion"));
        assert!(!host_acceptable("get-discord.net", "Discord"));
        assert!(!host_acceptable("krita.evil.com", "Krita"));
        assert!(!host_acceptable("evil.example.com", "Krita"));
    }

    #[test]
    fn hex64_validation() {
        assert!(is_hex64(&"a".repeat(64)));
        assert!(!is_hex64(&"a".repeat(63)));
        assert!(!is_hex64(&"g".repeat(64)));
    }

    #[test]
    fn version_compare_and_classify() {
        assert_eq!(version_cmp("2.0.0", "1.9.9"), Some(std::cmp::Ordering::Greater));
        assert_eq!(version_cmp("1.0", "1.0.0"), Some(std::cmp::Ordering::Equal));
        assert_eq!(version_cmp("v2.1", "v2.0"), Some(std::cmp::Ordering::Greater));
        assert_eq!(classify_version("2.0.0", "2.0.0"), "verified");
        assert_eq!(classify_version("2.1.0", "2.0.0"), "verified"); // newer than expected is fine
        assert_eq!(classify_version("1.0.0", "2.0.0"), "mismatch"); // still the old version
        assert_eq!(classify_version("Unknown", "2.0.0"), "unverified");
    }

    #[test]
    fn is_newer_only_accepts_strictly_higher() {
        // The Revision Tool regression: an AI "2.7.5" must NOT count as an update
        // over an installed "2.9.0", nor over an equal version.
        assert!(!is_newer("2.7.5", "2.9.0"));
        assert!(!is_newer("2.9.0", "2.9.0"));
        assert!(!is_newer("2.7.1", "2.7.5"));
        // A genuinely newer release is reported; `v`-prefixes are normalized.
        assert!(is_newer("2.7.5", "2.7.1"));
        assert!(is_newer("v2.9.1", "2.9.0"));
        // Empty latest is never an update; unknown current lets a real latest pass.
        assert!(!is_newer("", "2.9.0"));
        assert!(is_newer("2.9.0", ""));
        // Dotted dates still compare numerically (newer minor wins).
        assert!(is_newer("2024.02", "2024.01"));
        // Truly non-numeric tags fall back to a string inequality: equal is never
        // an update, differing is.
        assert!(!is_newer("nightly", "nightly"));
        assert!(is_newer("build-b", "build-a"));
    }

    #[test]
    fn match_installed_resolves_fuzzy_names() {
        let mut installed = std::collections::HashMap::new();
        installed.insert("revision tool version 2.9.0".to_string(), "2.9.0".to_string());
        installed.insert("krita".to_string(), "5.2.0".to_string());
        // The AI echoes a clean "Revision Tool"; it must still resolve to 2.9.0.
        assert_eq!(match_installed(&installed, "Revision Tool"), Some(&"2.9.0".to_string()));
        // Exact (case-insensitive) hit.
        assert_eq!(match_installed(&installed, "Krita"), Some(&"5.2.0".to_string()));
        // No overlap -> no match (caller falls back to the AI's own `current`).
        assert_eq!(match_installed(&installed, "Obsidian"), None);
    }
}
