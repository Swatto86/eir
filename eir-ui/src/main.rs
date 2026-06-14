#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pipe_client;
mod updates;

use pipe_client::SharedStatus;
use eir_proto::{SettingsUpdate, StatusPayload, UiMsg};
use std::sync::{Arc, Mutex};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Manager, State, WindowEvent,
};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::mpsc;
use tracing::error;

// ── Managed state ─────────────────────────────────────────────────────────────

/// Sender for UI commands (approve, toggle_pause) to the pipe client.
struct UiCmdTx(mpsc::Sender<UiMsg>);

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
async fn update_settings(settings: SettingsUpdate, tx: State<'_, UiCmdTx>) -> Result<(), String> {
    tx.0.send(UiMsg::UpdateSettings(Box::new(settings)))
        .await
        .map_err(|e| e.to_string())
}

// ── Tray helpers ──────────────────────────────────────────────────────────────

/// The app icon (dark shield + green "E"), decoded to RGBA once at startup.
struct IconBase {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

const ICON_PNG: &[u8] = include_bytes!("../../icons/128x128.png");

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
        "Active" => None, // original green icon — exactly like the app icon
        "Warning" => Some([234, 179, 8]),       // amber
        "PendingApproval" => Some([249, 115, 22]), // orange
        "Executing" => Some([59, 130, 246]),    // blue
        "Error" | "ServiceDisconnected" => Some([239, 68, 68]), // red
        _ => Some([107, 114, 128]),             // grey (connecting / unknown)
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
    Image::new_owned(pixels, base.width, base.height)
}

fn update_tray(tray: &TrayIcon<tauri::Wry>, base: &IconBase, status: &str) {
    let _ = tray.set_icon(Some(make_icon(base, status)));
    let _ = tray.set_tooltip(Some(&format!("Eir — {status}")));
}

// ── Auto-update ─────────────────────────────────────────────────────────────────

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
    let updater = match handle.updater() {
        Ok(u) => u,
        Err(e) => {
            error!("Updater unavailable: {e}");
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(status)
        .manage(UiCmdTx(ui_cmd_tx))
        .setup(move |app| {
            let icon_base = Arc::new(decode_icon());

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
            updates::list_app_updates,
            updates::update_app,
            updates::update_all_apps,
            updates::check_ai_updates,
            updates::check_app_update,
            updates::gbp_per_usd,
            updates::open_url,
            updates::set_app_note
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| error!("Eir UI failed: {e}"))
}
