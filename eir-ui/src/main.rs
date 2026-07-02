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
    sync::{Arc, Mutex},
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
    status.lock().unwrap().clone()
}

#[tauri::command]
async fn decide_approval(id: u64, approved: bool, tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::Approve { id, approved })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn toggle_pause(tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::TogglePause)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_problems(tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::ClearProblems)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_executions(tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::ClearExecutions)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_settings(settings: SettingsUpdate, tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::UpdateSettings(Box::new(settings)))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn run_updates_now(tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::RunUpdatesNow)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_update_history(tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::ClearUpdateHistory)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_updater_settings(
    settings: UpdaterSettingsUpdate,
    tx: State<'_, UiCmdTx>,
) -> Result<(), String> {
    tx.0.send(UiMsg::UpdateUpdaterSettings(Box::new(settings)))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_app_ignore(
    id: String,
    ignore: bool,
    note: String,
    tx: State<'_, UiCmdTx>,
) -> Result<(), String> {
    tx.0.send(UiMsg::SetAppIgnore { id, ignore, note })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_learned_fact(id: i64, op: String, tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::SetLearnedFact { id, op })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_advisor_settings(
    settings: AdvisorSettingsUpdate,
    tx: State<'_, UiCmdTx>,
) -> Result<(), String> {
    tx.0.send(UiMsg::SetAdvisorSettings(Box::new(settings)))
        .await
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
    use std::sync::atomic::Ordering;
    if UPDATE_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return Ok("An update is already downloading.".to_string());
    }
    let result = check_updates_inner(&handle).await;
    UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
    result
}

async fn check_updates_inner(handle: &AppHandle) -> Result<String, String> {
    let updater = handle.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => {
            update
                .download_and_install(|_, _| {}, || {})
                .await
                .map_err(|e| e.to_string())?;
            handle.restart();
        }
        Ok(None) => Ok("You're on the latest version.".to_string()),
        Err(e) => Err(e.to_string()),
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

fn update_tray(tray: &TrayIcon<tauri::Wry>, base: &IconBase, status: &str) {
    let _ = tray.set_icon(Some(make_icon(base, status)));
    let _ = tray.set_tooltip(Some(&format!("Eir — {status}")));
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
    use std::sync::atomic::Ordering;
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
    match updater.check().await {
        Ok(Some(update)) => {
            if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
                error!("Update install failed: {e}");
            } else {
                handle.restart();
            }
        }
        Ok(None) => {}
        Err(e) => error!("Update check failed: {e}"),
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

    let status_for_loop = status.clone();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Eir")
                .arg(AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(status)
        .manage(UiCmdTx(ui_cmd_tx))
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
                                let _ = w.set_focus();
                            }
                        }
                        "pause" => {
                            let tx = tx.clone();
                            tauri::async_runtime::spawn(async move {
                                let _ = tx.send(UiMsg::TogglePause).await;
                            });
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
                pipe_client::run(status_pipe, ui_cmd_rx).await;
            });

            let status_tray = status_for_loop.clone();
            let icon_for_loop = icon_base.clone();
            tauri::async_runtime::spawn(async move {
                let mut last = String::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let current = status_tray.lock().unwrap().status.clone();
                    if current != last {
                        last = current.clone();
                        update_tray(&tray, &icon_for_loop, &current);
                    }
                }
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
