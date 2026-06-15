//! App-update monitoring via winget. Listing runs unelevated; applying an update
//! runs winget elevated through `Start-Process -Verb RunAs` (one UAC prompt) so
//! machine-scope packages can be installed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
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

/// Split a winget table row on runs of 2+ spaces, preserving single spaces that
/// occur inside a field (e.g. "7-Zip 25.01 (x64)").
fn split_columns(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut cur = String::new();
    let mut spaces = 0usize;
    for ch in line.chars() {
        if ch == ' ' {
            spaces += 1;
        } else {
            if spaces >= 2 && !cur.is_empty() {
                cols.push(cur.trim().to_string());
                cur.clear();
            } else if spaces == 1 && !cur.is_empty() {
                cur.push(' ');
            }
            spaces = 0;
            cur.push(ch);
        }
    }
    if !cur.trim().is_empty() {
        cols.push(cur.trim().to_string());
    }
    cols
}

/// Parse `winget upgrade` output. Skips the progress-noise and header by starting
/// after the dashes separator, and stops at the "N upgrades available" footer.
fn parse_upgrades(text: &str) -> Vec<AppUpdate> {
    let mut updates = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            // The separator line is a run of dashes (possibly after stray noise).
            if trimmed.starts_with("---") || trimmed.ends_with("---") || trimmed.contains("-----")
            {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        // Footer, e.g. "21 upgrades available." — also stops at the "explicit
        // targeting" sub-table that some winget versions append.
        let lower = trimmed.to_lowercase();
        if lower.contains("upgrade") && lower.contains("available")
            || lower.starts_with("the following packages")
        {
            break;
        }
        let cols = split_columns(trimmed);
        if cols.len() < 4 {
            continue;
        }
        let name = cols[0].clone();
        // Strip the truncation ellipsis winget adds to long ids.
        let id = cols[1]
            .trim_end_matches('…')
            .trim_end_matches('.')
            .to_string();
        let current = cols[2].clone();
        let available = cols[3].clone();
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

/// Run an elevated winget command via UAC, waiting for it to finish.
async fn run_winget_elevated(args: Vec<String>) -> Result<String, String> {
    // Build a single-quoted PowerShell argument list; '' escapes a quote.
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$p = Start-Process winget -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList {arg_list}; exit $p.ExitCode"
    );
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if status.success() {
        Ok("ok".to_string())
    } else {
        Err(format!(
            "winget exited with code {}",
            status.code().unwrap_or(-1)
        ))
    }
}

#[tauri::command]
pub async fn update_app(id: String) -> Result<String, String> {
    run_winget_elevated(vec![
        "upgrade".into(),
        "--id".into(),
        id,
        "--silent".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
        "--disable-interactivity".into(),
    ])
    .await
}

#[tauri::command]
pub async fn update_all_apps() -> Result<String, String> {
    run_winget_elevated(vec![
        "upgrade".into(),
        "--all".into(),
        "--silent".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
        "--disable-interactivity".into(),
    ])
    .await
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
        ".net",
        "directx",
        "nvidia",
        "realtek",
        "intel(r)",
        "host app",
        "web experience",
        "microsoft 365",
        "office",
        "visual studio installer",
        "onedrive",
        "teams machine-wide",
        "eir",
    ];
    SKIP.iter().any(|s| n.contains(s))
}

/// Parse `winget list` for apps with no package source — rows that do NOT end in
/// "winget"/"msstore" can't be updated by winget. Returns (name, version).
fn parse_unmanaged(text: &str) -> Vec<(String, String)> {
    let mut apps = Vec::new();
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
        let cols = split_columns(trimmed);
        if cols.len() < 3 {
            continue;
        }
        let last = cols.last().map(|s| s.as_str()).unwrap_or("");
        if last.eq_ignore_ascii_case("winget") || last.eq_ignore_ascii_case("msstore") {
            continue; // winget-managed — handled by the winget panel
        }
        let name = cols[0].clone();
        let version = cols[2].clone();
        if name.is_empty() || version.is_empty() || is_noise(&name) {
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
            return after.find(close).map(|e| &after[..e]).unwrap_or(after).trim();
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
            .args(["list", "--accept-source-agreements", "--disable-interactivity"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    let notes = load_notes();
    let mut apps = parse_unmanaged(&String::from_utf8_lossy(&list_out.stdout));
    // Drop apps the user has chosen to ignore.
    apps.retain(|(n, _)| !notes.get(&n.to_lowercase()).map(|x| x.ignore).unwrap_or(false));
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

    let (updates, cost) = run_ai_check(prompt).await?;
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
APP: {name} ({current}){note_line}"
    );
    let (updates, cost) = run_ai_check(prompt).await?;
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
    let cfg = resolve_ai_cfg();
    if cfg.provider == "openrouter" {
        let key = cfg.openrouter_api_key.unwrap_or_default();
        if key.trim().is_empty() {
            return Err("OpenRouter is selected but no API key is set — add it in Settings.".into());
        }
        let model = if cfg.model.trim().is_empty() {
            "openrouter/free".to_string()
        } else {
            cfg.model.trim().to_string()
        };
        run_openrouter_check(prompt, &model, key.trim()).await
    } else {
        // Claude CLI (also the fallback for anthropic / openai_compatible, which
        // don't have a built-in web path here). update_check_model -> haiku.
        run_claude_check(prompt, &cfg.update_check_model).await
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
async fn run_openrouter_check(
    prompt: String,
    model: &str,
    key: &str,
) -> Result<(Vec<AiUpdate>, f64), String> {
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

    let json = extract_json(&content);
    let resp_updates: AiResp =
        serde_json::from_str(json).map_err(|e| format!("could not parse update list: {e}"))?;
    let updates = resp_updates
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

async fn run_claude_check(prompt: String, model: &str) -> Result<(Vec<AiUpdate>, f64), String> {
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

    let mut child = cmd.spawn().map_err(|e| format!("failed to run claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(420), child.wait_with_output())
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
    let result_text = env.result.unwrap_or_default();
    let json = strip_fences(&result_text);
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

    #[test]
    fn parses_winget_table_with_spaces_and_truncation() {
        let sample = "\
   -    progress noise    Name                Id                  Version    Available    Source
-----------------------------------------------------------------------------------------------
7-Zip 25.01 (x64)         7zip.7zip           25.01      26.01        winget
Visual Studio Build Tools 2022    Microsoft.VisualStudio.2022.Buil…   17.14.25 (January 2026)    17.14.34    winget
21 upgrades available.";
        let ups = parse_upgrades(sample);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].id, "7zip.7zip");
        assert_eq!(ups[0].name, "7-Zip 25.01 (x64)");
        assert_eq!(ups[0].available, "26.01");
        // Ellipsis stripped from the truncated id.
        assert_eq!(ups[1].id, "Microsoft.VisualStudio.2022.Buil");
        assert_eq!(ups[1].current, "17.14.25 (January 2026)");
    }

    #[test]
    fn unmanaged_apps_exclude_winget_managed_and_noise() {
        let sample = "\
Name                       Id                              Version       Available   Source
-------------------------------------------------------------------------------------------
7-Zip 25.01 (x64)          7zip.7zip                       25.01         26.01       winget
Git                        ARP\\Machine\\X64\\Git_is1          2.52.0
NVIDIA Graphics Driver     ARP\\Machine\\X64\\{B2FE1952}       596.49
iCloud Outlook             ARP\\Machine\\X64\\{81FA1580}       15.7.0.56";
        let apps = parse_unmanaged(sample);
        // 7-Zip excluded (winget), NVIDIA Driver excluded (noise); Git + iCloud kept.
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Git"));
        assert!(names.contains(&"iCloud Outlook"));
        assert!(!names.contains(&"7-Zip 25.01 (x64)"));
        assert!(!names.iter().any(|n| n.contains("NVIDIA")));
    }

    #[test]
    fn non_claude_model_falls_back_to_haiku() {
        // Blank and non-Claude ids become haiku; Claude aliases/ids pass through.
        assert_eq!(claude_model_or_haiku(""), "haiku");
        assert_eq!(claude_model_or_haiku("  "), "haiku");
        assert_eq!(claude_model_or_haiku("nex-agi/nex-n2-pro:free"), "haiku");
        assert_eq!(claude_model_or_haiku("nvidia/nemotron-3-super-120b-a12b:free"), "haiku");
        assert_eq!(claude_model_or_haiku("haiku"), "haiku");
        assert_eq!(claude_model_or_haiku("Sonnet"), "Sonnet");
        assert_eq!(claude_model_or_haiku("opus"), "opus");
        assert_eq!(claude_model_or_haiku("claude-haiku-4-5"), "claude-haiku-4-5");
    }
}
