//! Small UI-side helpers that aren't part of the (now service-side) update engine:
//! the USD→GBP rate for the cost display, and opening an external URL.

use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW — keep the spawned powershell hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Current USD→GBP rate for displaying costs in pounds, with a sensible offline
/// fallback.
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
