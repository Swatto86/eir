mod pipe_client;

use pipe_client::SharedStatus;
use sentry_proto::{StatusPayload, UiMsg};
use std::sync::{Arc, Mutex};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Manager, State,
};
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

// ── Tray helpers ──────────────────────────────────────────────────────────────

fn status_rgba(status: &str) -> Vec<u8> {
    let fill: [u8; 4] = match status {
        "Active" => [34, 197, 94, 255],
        "Warning" => [234, 179, 8, 255],
        "PendingApproval" => [249, 115, 22, 255],
        "Executing" => [59, 130, 246, 255],
        "Error" => [239, 68, 68, 255],
        "ServiceDisconnected" => [239, 68, 68, 255],
        _ => [107, 114, 128, 255],
    };
    let n = 16usize;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let i = (y * n + x) * 4;
            if x == 0 || x == n - 1 || y == 0 || y == n - 1 {
                px[i] = fill[0] / 2;
                px[i + 1] = fill[1] / 2;
                px[i + 2] = fill[2] / 2;
                px[i + 3] = 255;
            } else {
                px[i..i + 4].copy_from_slice(&fill);
            }
        }
    }
    px
}

fn make_icon(status: &str) -> Image<'static> {
    Image::new_owned(status_rgba(status), 16, 16)
}

fn update_tray(tray: &TrayIcon<tauri::Wry>, status: &str) {
    let _ = tray.set_icon(Some(make_icon(status)));
    let _ = tray.set_tooltip(Some(&format!("Sentry — {status}")));
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let status: SharedStatus = Arc::new(Mutex::new(StatusPayload {
        status: "Connecting".to_string(),
        error: Some("Connecting to Sentry service…".to_string()),
        ..Default::default()
    }));
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiMsg>(16);

    let status_for_loop = status.clone();

    tauri::Builder::default()
        .manage(status)
        .manage(UiCmdTx(ui_cmd_tx))
        .setup(move |app| {
            let open_item = MenuItem::with_id(app, "open", "Open Status", true, None::<&str>)?;
            let pause_item =
                MenuItem::with_id(app, "pause", "Pause Monitoring", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Sentry", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;

            let tray = TrayIconBuilder::new()
                .icon(make_icon("Connecting"))
                .tooltip("Sentry — Connecting")
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
            tauri::async_runtime::spawn(async move {
                let mut last = String::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let current = status_tray.lock().unwrap().status.clone();
                    if current != last {
                        last = current.clone();
                        update_tray(&tray, &current);
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            decide_approval,
            toggle_pause
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| error!("Sentry UI failed: {e}"))
}
