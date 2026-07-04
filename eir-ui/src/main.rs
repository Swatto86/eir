#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pipe_client;
mod util;

use eir_proto::{
    AdvisorSettingsUpdate, SettingsUpdate, StatusPayload, UiMsg, UpdaterSettingsUpdate,
};
use pipe_client::SharedStatus;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::mpsc;
use tracing::{error, warn};

// ── Managed state ─────────────────────────────────────────────────────────────

/// Sender for UI commands (approve, toggle_pause) to the pipe client.
struct UiCmdTx(mpsc::Sender<UiMsg>);

/// True while the pipe client holds a live connection to the service. Commands
/// that cross the pipe are refused when this is false, so a click made while the
/// service is down (or restarting) fails loudly instead of being queued and then
/// silently dropped when the dead connection's command backlog is drained.
struct ConnState(Arc<AtomicBool>);

/// Reject a pipe command when the service is disconnected. The UI's catch paths
/// then re-enable the buttons / show "Failed: …" instead of leaving a control
/// dead until restart.
fn ensure_connected(conn: &AtomicBool) -> Result<(), String> {
    if conn.load(Ordering::Relaxed) {
        Ok(())
    } else {
        Err("Eir service is not connected".to_string())
    }
}

const AUTOSTART_ARG: &str = "--hidden";
const UI_PREFERENCES_FILE: &str = "ui-preferences.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct UiPreferences {
    #[serde(default = "default_autostart_enabled")]
    autostart_enabled: bool,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            autostart_enabled: default_autostart_enabled(),
        }
    }
}

fn default_autostart_enabled() -> bool {
    true
}

fn preferences_path(handle: &AppHandle) -> Result<PathBuf, String> {
    let dir = handle.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("Create config directory: {e}"))?;
    Ok(dir.join(UI_PREFERENCES_FILE))
}

fn load_preferences(handle: &AppHandle) -> UiPreferences {
    let path = match preferences_path(handle) {
        Ok(path) => path,
        Err(e) => {
            warn!("Could not resolve UI preferences path: {e}");
            return UiPreferences::default();
        }
    };

    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            warn!("Could not parse UI preferences: {e}");
            UiPreferences::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UiPreferences::default(),
        Err(e) => {
            warn!("Could not read UI preferences: {e}");
            UiPreferences::default()
        }
    }
}

fn save_preferences(handle: &AppHandle, preferences: &UiPreferences) -> Result<(), String> {
    let path = preferences_path(handle)?;
    let raw = serde_json::to_string_pretty(preferences).map_err(|e| e.to_string())?;
    fs::write(path, raw).map_err(|e| format!("Save UI preferences: {e}"))
}

fn apply_autostart_preference(handle: &AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = handle.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())?;
    } else {
        manager.disable().map_err(|e| e.to_string())?;
    }
    manager.is_enabled().map_err(|e| e.to_string())
}

fn sync_autostart_on_startup(handle: &AppHandle) {
    let preferences = load_preferences(handle);
    if let Err(e) = apply_autostart_preference(handle, preferences.autostart_enabled) {
        warn!("Could not apply autostart preference: {e}");
    }
}

fn launched_hidden() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_ARG)
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_status(status: State<'_, SharedStatus>) -> StatusPayload {
    // Recover from a poisoned lock instead of panicking: the guarded data is a plain
    // snapshot, so a prior panic elsewhere shouldn't turn every 2s poll into a crash.
    pipe_client::lock_status(&status).clone()
}

#[tauri::command]
async fn decide_approval(
    id: u64,
    approved: bool,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::Approve { id, approved })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn toggle_pause(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::TogglePause).map_err(|e| e.to_string())
}

#[tauri::command]
async fn undo_registry(
    id: i64,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::UndoRegistry { id })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn ask_eir(
    question: String,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::AskEir { question })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn scan_disk(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::ScanDisk).map_err(|e| e.to_string())
}

#[tauri::command]
async fn clean_disk_entry(
    id: String,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::CleanDiskEntry { id })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn scan_startup(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::ScanStartup).map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_startup_entry(
    id: String,
    enable: bool,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::SetStartupEntry { id, enable })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_problems(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::ClearProblems)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_executions(
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::ClearExecutions)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_settings(
    settings: SettingsUpdate,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::UpdateSettings(Box::new(settings)))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn run_updates_now(tx: State<'_, UiCmdTx>, conn: State<'_, ConnState>) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::RunUpdatesNow)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_update_history(
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::ClearUpdateHistory)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_updater_settings(
    settings: UpdaterSettingsUpdate,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::UpdateUpdaterSettings(Box::new(settings)))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_app_ignore(
    id: String,
    ignore: bool,
    note: String,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::SetAppIgnore { id, ignore, note })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_learned_fact(
    id: i64,
    op: String,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::SetLearnedFact { id, op })
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_advisor_settings(
    settings: AdvisorSettingsUpdate,
    tx: State<'_, UiCmdTx>,
    conn: State<'_, ConnState>,
) -> Result<(), String> {
    ensure_connected(&conn.0)?;
    tx.0.try_send(UiMsg::SetAdvisorSettings(Box::new(settings)))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_app_version(handle: AppHandle) -> String {
    handle.package_info().version.to_string()
}

/// On-demand update check for the About view. Installs and relaunches when a
/// newer signed release exists (mirroring the background checker), otherwise
/// reports the current state as a string for the UI to display.
#[tauri::command]
async fn check_updates_now(handle: AppHandle) -> Result<String, String> {
    if UPDATE_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return Ok("An update is already downloading.".to_string());
    }
    let result = check_updates_inner(&handle).await;
    UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
    result
}

async fn check_updates_inner(handle: &AppHandle) -> Result<String, String> {
    use std::time::Duration;
    let updater = handle.updater().map_err(|e| e.to_string())?;
    // Timeouts guarantee the future completes so UPDATE_IN_PROGRESS is always reset —
    // a stalled network check/download would otherwise latch it true forever, killing
    // both this command and the background checker until the app is restarted.
    let found = tokio::time::timeout(Duration::from_secs(120), updater.check())
        .await
        .map_err(|_| "update check timed out".to_string())?
        .map_err(|e| e.to_string())?;
    match found {
        Some(update) => {
            tokio::time::timeout(
                Duration::from_secs(600),
                update.download_and_install(|_, _| {}, || {}),
            )
            .await
            .map_err(|_| "update download timed out".to_string())?
            .map_err(|e| e.to_string())?;
            handle.restart();
        }
        None => Ok("You're on the latest version.".to_string()),
    }
}

#[tauri::command]
fn get_autostart_enabled(handle: AppHandle) -> Result<bool, String> {
    handle.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_autostart_enabled(enabled: bool, handle: AppHandle) -> Result<bool, String> {
    let preferences = UiPreferences {
        autostart_enabled: enabled,
    };
    save_preferences(&handle, &preferences)?;
    apply_autostart_preference(&handle, enabled)
}

// ── Tray helpers ──────────────────────────────────────────────────────────────

/// The app icon (dark shield + green "E"), decoded to RGBA once at startup.
struct IconBase {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

// Source the tray icon from the 256px asset and downsample with a quality
// filter (see make_icon) — gives Windows a small, pre-filtered icon so its own
// tray scaling barely runs, instead of a jagged 128px→~20px squash.
const ICON_PNG: &[u8] = include_bytes!("../../icons/128x128@2x.png");

/// Pixel size handed to Windows for the tray icon. 32 keeps it crisp in the
/// overflow flyout (~28–32px) and only mildly downscaled in the small tray.
const TRAY_ICON_PX: u32 = 32;

fn decode_icon() -> IconBase {
    let img = image::load_from_memory(ICON_PNG)
        .expect("embedded icon must decode")
        .to_rgba8();
    let (width, height) = img.dimensions();
    IconBase {
        rgba: img.into_raw(),
        width,
        height,
    }
}

/// The accent colour to paint the shield/"S" for a given status.
/// `None` means leave the icon untouched (the original green app icon).
fn status_accent(status: &str) -> Option<[u8; 3]> {
    match status {
        "Active" => None,                 // original green icon — exactly like the app icon
        "Warning" => Some([234, 179, 8]), // amber
        "PendingApproval" => Some([249, 115, 22]), // orange
        "Executing" => Some([59, 130, 246]), // blue
        "Error" | "ServiceDisconnected" => Some([239, 68, 68]), // red
        _ => Some([107, 114, 128]),       // grey (connecting / unknown)
    }
}

/// Repaint the bright foreground (the green border + "E") with `target`,
/// leaving the dark shield background and transparent pixels intact.
fn recolor(base: &IconBase, target: [u8; 3]) -> Vec<u8> {
    let mut out = base.rgba.clone();
    for px in out.chunks_exact_mut(4) {
        if px[3] < 16 {
            continue; // transparent
        }
        // Foreground accent pixels are the bright ones; the shield bg is dark.
        if px[0].max(px[1]).max(px[2]) > 80 {
            px[0] = target[0];
            px[1] = target[1];
            px[2] = target[2];
        }
    }
    out
}

fn make_icon(base: &IconBase, status: &str) -> Image<'static> {
    let pixels = match status_accent(status) {
        None => base.rgba.clone(),
        Some(target) => recolor(base, target),
    };
    // Downsample from the full-res (recoloured) image to the tray size with a
    // high-quality filter so the edges stay smooth at small sizes.
    let src = image::RgbaImage::from_raw(base.width, base.height, pixels)
        .expect("icon buffer matches its dimensions");
    let scaled = image::imageops::resize(
        &src,
        TRAY_ICON_PX,
        TRAY_ICON_PX,
        image::imageops::FilterType::Lanczos3,
    );
    Image::new_owned(scaled.into_raw(), TRAY_ICON_PX, TRAY_ICON_PX)
}

/// Space the internal CamelCase boundaries of a status word for display, mirroring
/// the UI header ("PendingApproval" → "Pending Approval"). Single words are unchanged.
fn friendly_status(status: &str) -> String {
    let mut out = String::with_capacity(status.len() + 4);
    for (i, c) in status.char_indices() {
        if i > 0 && c.is_ascii_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Repaint the tray icon + tooltip. Returns false when either write failed so the
/// caller can retry next tick — otherwise a transient shell failure sticks forever
/// because the change-guard then sees `last == current` and never repaints.
fn update_tray(tray: &TrayIcon<tauri::Wry>, base: &IconBase, status: &str) -> bool {
    let icon_ok = tray.set_icon(Some(make_icon(base, status))).is_ok();
    let tip_ok = tray
        .set_tooltip(Some(&format!("Eir — {}", friendly_status(status))))
        .is_ok();
    icon_ok && tip_ok
}

// ── Approval notifications ──────────────────────────────────────────────────────

/// Poll the cached status and fire an OS notification the first time each pending
/// approval id appears. Approvals already pending when the UI starts are primed into
/// the seen-set without alerting (no stale burst on launch); ids that leave the queue
/// are forgotten so a genuinely new proposal always notifies.
async fn notify_on_new_approvals(status: SharedStatus, handle: AppHandle) {
    use std::collections::HashSet;
    use tauri_plugin_notification::NotificationExt;

    let mut seen: HashSet<u64> = HashSet::new();
    let mut primed = false;
    let mut last_digest_at: i64 = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let (pending, digest_at): (Vec<(u64, String)>, i64) = {
            let s = pipe_client::lock_status(&status);
            let pending = s
                .pending_approvals
                .iter()
                .map(|a| {
                    let text = if a.action_summary.is_empty() {
                        a.diagnosis.clone()
                    } else {
                        a.action_summary.clone()
                    };
                    (a.id, text)
                })
                .collect();
            let digest_at = s.digest.as_ref().map(|d| d.generated_at).unwrap_or(0);
            (pending, digest_at)
        };

        // Notify once when a new weekly digest lands. `last_digest_at` is seeded from
        // the startup value on the priming pass below (before its `continue`), so an
        // existing digest on launch doesn't alert — but the FIRST-ever digest (seeded
        // 0 → real) still does. (Dropping the old `last_digest_at != 0` guard, which
        // swallowed that first notification on every fresh install.)
        if primed && digest_at > last_digest_at {
            if let Err(e) = handle
                .notification()
                .builder()
                .title("Eir — weekly health digest")
                .body("Your weekly system summary is ready in Eir.")
                .show()
            {
                warn!("Failed to show digest notification: {e}");
            }
        }
        last_digest_at = digest_at;

        let current: HashSet<u64> = pending.iter().map(|(id, _)| *id).collect();
        if !primed {
            seen = current;
            primed = true;
            continue;
        }

        for (id, text) in &pending {
            if seen.insert(*id) {
                let body = if text.trim().is_empty() {
                    "A fix needs your approval.".to_string()
                } else {
                    text.clone()
                };
                if let Err(e) = handle
                    .notification()
                    .builder()
                    .title("Eir — approval needed")
                    .body(&body)
                    .show()
                {
                    warn!("Failed to show approval notification: {e}");
                }
            }
        }
        // Drop ids no longer pending so the set can't grow without bound and a later
        // re-proposal would notify again.
        seen.retain(|id| current.contains(id));
    }
}

// ── Auto-update ─────────────────────────────────────────────────────────────────

/// Guards against the background checker and the About view's manual check
/// running download_and_install concurrently (two installers racing).
static UPDATE_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Check for updates shortly after startup and every 6 hours thereafter. When a
/// newer signed release is published, download, install (runs the NSIS installer,
/// which prompts for elevation and updates the service too), and relaunch.
fn spawn_update_checker(handle: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        loop {
            check_for_update(&handle).await;
            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });
}

async fn check_for_update(handle: &tauri::AppHandle) {
    if UPDATE_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return; // a manual check is already installing
    }
    let updater = match handle.updater() {
        Ok(u) => u,
        Err(e) => {
            error!("Updater unavailable: {e}");
            UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
            return;
        }
    };
    // Timeouts guarantee completion so UPDATE_IN_PROGRESS is reset even if the check
    // or download stalls — otherwise the 6-hour loop wedges here and auto-update dies.
    match tokio::time::timeout(std::time::Duration::from_secs(120), updater.check()).await {
        Ok(Ok(Some(update))) => {
            match tokio::time::timeout(
                std::time::Duration::from_secs(600),
                update.download_and_install(|_, _| {}, || {}),
            )
            .await
            {
                Ok(Ok(())) => handle.restart(),
                Ok(Err(e)) => error!("Update install failed: {e}"),
                Err(_) => error!("Update download timed out"),
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => error!("Update check failed: {e}"),
        Err(_) => error!("Update check timed out"),
    }
    UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let status: SharedStatus = Arc::new(Mutex::new(StatusPayload {
        status: "Connecting".to_string(),
        error: Some("Connecting to Eir service…".to_string()),
        ..Default::default()
    }));
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiMsg>(16);

    let connected: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let connected_for_pipe = connected.clone();

    let status_for_loop = status.clone();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Eir")
                .arg(AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .manage(status)
        .manage(UiCmdTx(ui_cmd_tx))
        .manage(ConnState(connected))
        .setup(move |app| {
            let icon_base = Arc::new(decode_icon());
            let start_hidden = launched_hidden();

            sync_autostart_on_startup(app.handle());

            // Background auto-update: check on startup, then every 6 hours.
            // If a newer signed release exists, download, install, and relaunch.
            spawn_update_checker(app.handle().clone());

            let open_item = MenuItem::with_id(app, "open", "Open Status", true, None::<&str>)?;
            let pause_item =
                MenuItem::with_id(app, "pause", "Pause Monitoring", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Eir", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;

            let tray = TrayIconBuilder::new()
                .icon(make_icon(&icon_base, "Connecting"))
                .tooltip("Eir — Connecting")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event({
                    let tx = app.state::<UiCmdTx>().0.clone();
                    move |app, event| match event.id.as_ref() {
                        "open" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                        "pause" => {
                            // Gate on the connected flag like the window commands: a tray
                            // click while the service is down/restarting would otherwise
                            // queue silently (a menu item can't surface an error) and could
                            // replay as a stale toggle on reconnect.
                            let conn = app.state::<ConnState>();
                            if ensure_connected(&conn.0).is_ok() {
                                let tx = tx.clone();
                                tauri::async_runtime::spawn(async move {
                                    let _ = tx.send(UiMsg::TogglePause).await;
                                });
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            if !start_hidden {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }

            // Background: pipe client + tray colour sync
            let status_pipe = status_for_loop.clone();
            tauri::async_runtime::spawn(async move {
                pipe_client::run(status_pipe, ui_cmd_rx, connected_for_pipe).await;
            });

            let status_tray = status_for_loop.clone();
            let icon_for_loop = icon_base.clone();
            let pause_item_tray = pause_item.clone();
            tauri::async_runtime::spawn(async move {
                let mut last = String::new();
                let mut last_paused: Option<bool> = None;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let (current, paused) = {
                        let s = pipe_client::lock_status(&status_tray);
                        (s.status.clone(), s.paused)
                    };
                    if current != last && update_tray(&tray, &icon_for_loop, &current) {
                        last = current.clone();
                    }
                    // Keep the tray menu's pause entry in step with the state, so it
                    // reads "Resume Monitoring" while paused instead of always offering
                    // to pause. Same retry-on-failure shape as the icon above.
                    if last_paused != Some(paused)
                        && pause_item_tray
                            .set_text(if paused {
                                "Resume Monitoring"
                            } else {
                                "Pause Monitoring"
                            })
                            .is_ok()
                    {
                        last_paused = Some(paused);
                    }
                }
            });

            // Background: OS notification when a NEW fix needs approval, so an
            // unattended, tray-resident Eir surfaces the ask instead of it only being
            // visible if the window is open.
            let status_notify = status_for_loop.clone();
            let notify_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                notify_on_new_approvals(status_notify, notify_handle).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it to the tray; the service keeps running.
            // Use "Quit Eir" from the tray menu to exit completely.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            decide_approval,
            toggle_pause,
            undo_registry,
            ask_eir,
            scan_disk,
            clean_disk_entry,
            scan_startup,
            set_startup_entry,
            update_settings,
            clear_problems,
            clear_executions,
            run_updates_now,
            clear_update_history,
            set_updater_settings,
            set_app_ignore,
            set_learned_fact,
            set_advisor_settings,
            get_app_version,
            check_updates_now,
            get_autostart_enabled,
            set_autostart_enabled,
            util::gbp_per_usd,
            util::open_url
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| error!("Eir UI failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_status_spaces_camelcase() {
        assert_eq!(friendly_status("PendingApproval"), "Pending Approval");
        assert_eq!(
            friendly_status("ServiceDisconnected"),
            "Service Disconnected"
        );
        assert_eq!(friendly_status("Active"), "Active");
        assert_eq!(friendly_status(""), "");
    }

    #[test]
    fn ensure_connected_reflects_flag() {
        let flag = AtomicBool::new(false);
        assert!(ensure_connected(&flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(ensure_connected(&flag).is_ok());
    }
}
