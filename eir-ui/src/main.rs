#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ask_attach;
mod game_detect;
mod pipe_client;
mod provider_models;
mod util;

use eir_proto::{
    AdvisorSettingsUpdate, CommandResult, SettingsUpdate, StatusPayload, UiMsg, UiRequest,
    UpdaterSettingsUpdate, CAP_COMMAND_RESULTS, CAP_PROVIDER_TEST, CAP_TARGETED_UPDATE_RETRY,
};
use pipe_client::{CommandWaiters, SharedStatus};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    webview::PageLoadEvent,
    AppHandle, Manager, State, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::mpsc;
use tracing::{error, warn};
use windows_service::{
    service::{ServiceAccess, ServiceState, ServiceStatus},
    service_manager::{ServiceManager, ServiceManagerAccess},
};

// ── Managed state ─────────────────────────────────────────────────────────────

/// Pipe sender plus the minimum correlation state needed to distinguish
/// "queued" on an old service from "applied" on a capable service.
struct UiCmdTx {
    sender: mpsc::Sender<UiRequest>,
    connected: Arc<AtomicBool>,
    status: SharedStatus,
    waiters: CommandWaiters,
    next_id: AtomicU64,
}

/// True while the pipe client holds a live connection to the service. Commands
/// that cross the pipe are refused when this is false, so a click made while the
/// service is down (or restarting) fails loudly instead of being queued and then
/// silently dropped when the dead connection's command backlog is drained.
struct ConnState(Arc<AtomicBool>);

/// Serialises startup synchronisation with user-initiated autostart changes. All work
/// guarded by this lock runs on Tauri's blocking pool, never the UI thread.
struct AutostartIo(Arc<Mutex<()>>);

#[derive(Clone, Copy)]
struct RuntimeMode {
    portable: bool,
}

fn portable_ui_mode_at(flag: Option<&OsStr>) -> bool {
    flag == Some(OsStr::new("1"))
}

fn portable_ui_mode() -> bool {
    portable_ui_mode_at(std::env::var_os("EIR_PORTABLE").as_deref())
}

fn installed_integrations_enabled(portable: bool) -> bool {
    !portable
}

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

fn supports(status: &SharedStatus, capability: &str) -> bool {
    pipe_client::lock_status(status)
        .capabilities
        .iter()
        .any(|x| x == capability)
}

fn command_result(result: CommandResult) -> Result<String, String> {
    let fallback = if result.ok {
        "Applied"
    } else {
        "Service rejected the command"
    };
    let message = if result.message.trim().is_empty() {
        fallback.to_string()
    } else {
        result.message
    };
    if result.ok {
        Ok(message)
    } else {
        Err(message)
    }
}

fn request_id_seed() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(u64::from(now.subsec_nanos()))
        .wrapping_add(u64::from(std::process::id()))
}

async fn send_command(tx: &UiCmdTx, command: UiMsg) -> Result<String, String> {
    send_command_with_timeout(tx, command, Duration::from_secs(30)).await
}

async fn send_command_with_timeout(
    tx: &UiCmdTx,
    command: UiMsg,
    timeout: Duration,
) -> Result<String, String> {
    ensure_connected(&tx.connected)?;
    if !supports(&tx.status, CAP_COMMAND_RESULTS) {
        tx.sender
            .try_send(command.into())
            .map_err(|e| e.to_string())?;
        return Ok("Queued — this service cannot confirm application".to_string());
    }

    let request_id = tx.next_id.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    pipe_client::lock_waiters(&tx.waiters).insert(request_id, reply_tx);
    if let Err(e) = tx.sender.try_send(UiRequest {
        request_id: Some(request_id),
        command,
    }) {
        pipe_client::lock_waiters(&tx.waiters).remove(&request_id);
        return Err(e.to_string());
    }

    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(result)) => command_result(result),
        Ok(Err(_)) => Err("Service disconnected before confirming the command".to_string()),
        Err(_) => {
            pipe_client::lock_waiters(&tx.waiters).remove(&request_id);
            Err("Service did not confirm the command in time".to_string())
        }
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

/// Migrate legacy reverse-DNS app-folder (`co.swatto.eir`) to the friendly
/// `Eir` folder under the user's config directory. The global Tauri standard
/// stores app state under `<AppName>`, not the bundle identifier. This is a
/// one-time rename on startup when the new folder is absent, so existing
/// users keep their preferences (notably the autostart setting).
fn migrate_app_folder(handle: &AppHandle) -> Result<(), String> {
    let old_dir = handle.path().app_config_dir().map_err(|e| e.to_string())?;
    let new_dir = handle
        .path()
        .config_dir()
        .map_err(|e| e.to_string())?
        .join("Eir");
    if old_dir.exists() && !new_dir.exists() {
        fs::rename(&old_dir, &new_dir).map_err(|e| {
            format!(
                "Migrate app folder from '{}' to '{}': {e}",
                old_dir.display(),
                new_dir.display()
            )
        })?;
    }
    Ok(())
}

fn preferences_path(handle: &AppHandle) -> Result<PathBuf, String> {
    let dir = handle
        .path()
        .config_dir()
        .map_err(|e| e.to_string())?
        .join("Eir");
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
) -> Result<String, String> {
    send_command(&tx, UiMsg::Approve { id, approved }).await
}

#[tauri::command]
async fn toggle_pause(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::TogglePause).await
}

#[tauri::command]
async fn undo_registry(id: i64, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::UndoRegistry { id }).await
}

#[tauri::command]
async fn ask_eir(
    question: String,
    tx: State<'_, UiCmdTx>,
    atts: State<'_, AskAttachments>,
) -> Result<String, String> {
    let attachments = take_ask_attachments(&atts)?;
    let result = send_command(
        &tx,
        UiMsg::AskEir {
            question,
            attachments: attachments.clone(),
        },
    )
    .await;
    if result.is_err() {
        restore_ask_attachments(&atts, attachments)?;
    }
    result
}

/// Pending Ask attachments, collected by the picker and consumed on the next `ask_eir`.
struct AskAttachments(std::sync::Mutex<Vec<eir_proto::AskAttachment>>);

fn take_ask_attachments(atts: &AskAttachments) -> Result<Vec<eir_proto::AskAttachment>, String> {
    Ok(std::mem::take(
        &mut *atts.0.lock().map_err(|e| e.to_string())?,
    ))
}

fn restore_ask_attachments(
    atts: &AskAttachments,
    mut failed: Vec<eir_proto::AskAttachment>,
) -> Result<(), String> {
    let mut pending = atts.0.lock().map_err(|e| e.to_string())?;
    failed.append(&mut pending);
    *pending = failed;
    Ok(())
}

#[tauri::command]
async fn add_ask_attachments(
    kind: String,
    app: AppHandle,
    atts: State<'_, AskAttachments>,
) -> Result<Vec<ask_attach::AttachmentMeta>, String> {
    let (remaining_count, remaining_bytes) = {
        let guard = atts.0.lock().map_err(|e| e.to_string())?;
        let used_bytes: usize = guard.iter().map(|a| a.content.len()).sum();
        (
            ask_attach::MAX_ATTACHMENTS.saturating_sub(guard.len()),
            ask_attach::MAX_TOTAL_BYTES.saturating_sub(used_bytes),
        )
    };
    if remaining_count == 0 || remaining_bytes == 0 {
        let guard = atts.0.lock().map_err(|e| e.to_string())?;
        return Ok(guard
            .iter()
            .map(|a| ask_attach::AttachmentMeta {
                name: a.name.clone(),
                kind: a.kind.clone(),
            })
            .collect());
    }
    let app2 = app.clone();
    // Dialog + file reads + image transcode all off the async runtime.
    let processed: Vec<eir_proto::AskAttachment> =
        tauri::async_runtime::spawn_blocking(move || {
            use tauri_plugin_dialog::DialogExt;
            if kind == "folder" {
                match app2.dialog().file().blocking_pick_folder() {
                    Some(fp) => fp
                        .into_path()
                        .ok()
                        .map(|p| ask_attach::process_folder(&p, remaining_count, remaining_bytes))
                        .unwrap_or_default(),
                    None => vec![],
                }
            } else {
                match app2.dialog().file().blocking_pick_files() {
                    Some(fps) => {
                        let paths: Vec<std::path::PathBuf> = fps
                            .into_iter()
                            .filter_map(|fp| fp.into_path().ok())
                            .collect();
                        ask_attach::process_files(&paths, remaining_count, remaining_bytes)
                    }
                    None => vec![],
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?;

    let mut guard = atts.0.lock().map_err(|e| e.to_string())?;
    let mut total: usize = guard.iter().map(|a| a.content.len()).sum();
    for a in processed {
        // Bound both the count and the TOTAL payload (across picks) so the eventual
        // AskEir line can't exceed the pipe cap and get silently dropped.
        if guard.len() >= ask_attach::MAX_ATTACHMENTS
            || total + a.content.len() > ask_attach::MAX_TOTAL_BYTES
        {
            break;
        }
        total += a.content.len();
        guard.push(a);
    }
    Ok(guard
        .iter()
        .map(|a| ask_attach::AttachmentMeta {
            name: a.name.clone(),
            kind: a.kind.clone(),
        })
        .collect())
}

#[tauri::command]
fn clear_ask_attachments(atts: State<'_, AskAttachments>) -> Result<(), String> {
    atts.0.lock().map_err(|e| e.to_string())?.clear();
    Ok(())
}

#[tauri::command]
fn remove_ask_attachment(
    index: usize,
    atts: State<'_, AskAttachments>,
) -> Result<Vec<ask_attach::AttachmentMeta>, String> {
    let mut guard = atts.0.lock().map_err(|e| e.to_string())?;
    if index < guard.len() {
        guard.remove(index);
    }
    Ok(guard
        .iter()
        .map(|a| ask_attach::AttachmentMeta {
            name: a.name.clone(),
            kind: a.kind.clone(),
        })
        .collect())
}

#[tauri::command]
async fn clear_ask(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ClearAsk).await
}

#[tauri::command]
async fn scan_disk(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ScanDisk).await
}

#[tauri::command]
async fn clean_disk_entry(id: String, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::CleanDiskEntry { id }).await
}

#[tauri::command]
async fn scan_startup(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ScanStartup).await
}

#[tauri::command]
async fn set_startup_entry(
    id: String,
    enable: bool,
    tx: State<'_, UiCmdTx>,
) -> Result<String, String> {
    send_command(&tx, UiMsg::SetStartupEntry { id, enable }).await
}

#[tauri::command]
async fn clear_problems(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ClearProblems).await
}

#[tauri::command]
async fn clear_executions(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ClearExecutions).await
}

#[tauri::command]
async fn refresh_status(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::RefreshStatus).await
}

#[tauri::command]
async fn set_gaming(on: bool, manual: bool, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::SetGaming { on, manual }).await
}

#[tauri::command]
async fn update_settings(
    settings: SettingsUpdate,
    tx: State<'_, UiCmdTx>,
) -> Result<String, String> {
    send_command(&tx, UiMsg::UpdateSettings(Box::new(settings))).await
}

#[tauri::command]
async fn test_provider(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    if !supports(&tx.status, CAP_PROVIDER_TEST) {
        return Err(
            "The running service does not support provider tests; update it first".to_string(),
        );
    }
    // Leave room for the service's own 120-second provider deadline to produce
    // and deliver its correlated timeout result.
    send_command_with_timeout(&tx, UiMsg::TestProvider, Duration::from_secs(135)).await
}

#[tauri::command]
async fn run_updates_now(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::RunUpdatesNow).await
}

#[tauri::command]
async fn retry_app_update(id: String, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    if id.trim().is_empty() || id.chars().count() > 200 {
        return Err("invalid app id".to_string());
    }
    if !supports(&tx.status, CAP_TARGETED_UPDATE_RETRY) {
        return Err(
            "The running service does not support individual update retries; update it first"
                .to_string(),
        );
    }
    send_command(&tx, UiMsg::RetryAppUpdate { id }).await
}

#[tauri::command]
async fn clear_update_history(tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::ClearUpdateHistory).await
}

#[tauri::command]
async fn set_updater_settings(
    settings: UpdaterSettingsUpdate,
    tx: State<'_, UiCmdTx>,
) -> Result<String, String> {
    send_command(&tx, UiMsg::UpdateUpdaterSettings(Box::new(settings))).await
}

#[tauri::command]
async fn set_app_ignore(
    id: String,
    ignore: bool,
    note: String,
    tx: State<'_, UiCmdTx>,
) -> Result<String, String> {
    send_command(&tx, UiMsg::SetAppIgnore { id, ignore, note }).await
}

#[tauri::command]
async fn set_app_note(id: String, note: String, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    if id.trim().is_empty() || id.chars().count() > 200 {
        return Err("invalid app id".to_string());
    }
    if note.chars().count() > 2_000 {
        return Err("app note is too long".to_string());
    }
    send_command(&tx, UiMsg::SetAppNote { id, note }).await
}

#[tauri::command]
async fn set_learned_fact(id: i64, op: String, tx: State<'_, UiCmdTx>) -> Result<String, String> {
    send_command(&tx, UiMsg::SetLearnedFact { id, op }).await
}

#[tauri::command]
async fn set_advisor_settings(
    settings: AdvisorSettingsUpdate,
    tx: State<'_, UiCmdTx>,
) -> Result<String, String> {
    send_command(&tx, UiMsg::SetAdvisorSettings(Box::new(settings))).await
}

#[tauri::command]
fn get_app_version(handle: AppHandle) -> String {
    handle.package_info().version.to_string()
}

#[tauri::command]
fn get_service_version(status: State<'_, SharedStatus>) -> Option<String> {
    pipe_client::lock_status(&status).svc_version.clone()
}

#[tauri::command]
fn is_portable(mode: State<'_, RuntimeMode>) -> bool {
    mode.portable
}

fn runtime_service_state(portable: bool, connected: bool) -> Option<&'static str> {
    portable.then_some(if connected { "running" } else { "stopped" })
}

/// Query the SCM directly for the Eir service state, independent of the pipe
/// connection. Used by the About view to offer an Install button when the
/// service is not registered.
fn query_service_state() -> Result<String, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("Service manager: {e}"))?;
    let svc = match manager.open_service("EirSvc", ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        // 1060 = ERROR_SERVICE_DOES_NOT_EXIST.
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => {
            return Ok("not_installed".to_string());
        }
        Err(e) => return Err(format!("Open service: {e}")),
    };
    let status: ServiceStatus = svc
        .query_status()
        .map_err(|e| format!("Query status: {e}"))?;
    let state = match status.current_state {
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::PausePending => "pausing",
        ServiceState::Paused => "paused",
        ServiceState::ContinuePending => "resuming",
    };
    Ok(state.to_string())
}

#[tauri::command]
async fn get_service_state(
    mode: State<'_, RuntimeMode>,
    connection: State<'_, ConnState>,
) -> Result<String, String> {
    if let Some(state) = runtime_service_state(
        mode.portable,
        connection.0.load(std::sync::atomic::Ordering::Relaxed),
    ) {
        return Ok(state.to_string());
    }
    tauri::async_runtime::spawn_blocking(query_service_state)
        .await
        .map_err(|e| e.to_string())?
}

/// Install the bundled service binary. The installer has already placed
/// `eir-svc.exe` next to the UI exe; this command re-runs that binary with
/// the `install` verb elevated via a UAC prompt.
fn service_install_command(svc: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "hidden",
        "-Command",
        "$ErrorActionPreference='Stop'; exit (Start-Process -Verb RunAs -FilePath $env:EIR_SERVICE_EXE -ArgumentList 'install' -Wait -PassThru).ExitCode",
    ])
    .env("EIR_SERVICE_EXE", svc);
    command
}

fn service_install_path_at(executable: &Path, portable: bool) -> Result<PathBuf, String> {
    if !installed_integrations_enabled(portable) {
        return Err("Service installation is unavailable in portable mode.".to_string());
    }
    executable
        .parent()
        .map(|directory| directory.join("eir-svc.exe"))
        .ok_or_else(|| "Could not resolve install directory".to_string())
}

#[tauri::command]
async fn install_service(mode: State<'_, RuntimeMode>) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Current exe: {e}"))?;
    let svc = service_install_path_at(&exe, mode.portable)?;
    if !svc.exists() {
        return Err(format!("Service binary not found: {}", svc.display()));
    }

    // Run PowerShell elevated and wait so the UI can refresh state once the
    // service is registered and started.
    let status = service_install_command(&svc)
        .status()
        .await
        .map_err(|e| format!("Failed to launch installer: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Service installer exited with code {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok("Service installed and started.".to_string())
}

/// On-demand update check for the About view. Installs and relaunches when a
/// newer signed release exists (mirroring the background checker), otherwise
/// reports the current state as a string for the UI to display.
#[tauri::command]
async fn check_updates_now(
    handle: AppHandle,
    mode: State<'_, RuntimeMode>,
) -> Result<String, String> {
    if !installed_integrations_enabled(mode.portable) {
        return Ok("Portable Eir updates by downloading the latest portable release.".to_string());
    }
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
async fn get_autostart_enabled(
    handle: AppHandle,
    io: State<'_, AutostartIo>,
    mode: State<'_, RuntimeMode>,
) -> Result<bool, String> {
    if !installed_integrations_enabled(mode.portable) {
        return Ok(false);
    }
    let io = io.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = io.lock().unwrap_or_else(|p| p.into_inner());
        handle.autolaunch().is_enabled().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_autostart_enabled(
    enabled: bool,
    handle: AppHandle,
    io: State<'_, AutostartIo>,
    mode: State<'_, RuntimeMode>,
) -> Result<bool, String> {
    if !installed_integrations_enabled(mode.portable) {
        return Err("Start with Windows is unavailable in portable mode.".to_string());
    }
    let io = io.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = io.lock().unwrap_or_else(|p| p.into_inner());
        let preferences = UiPreferences {
            autostart_enabled: enabled,
        };
        save_preferences(&handle, &preferences)?;
        apply_autostart_preference(&handle, enabled)
    })
    .await
    .map_err(|e| e.to_string())?
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

// Infallible: `ICON_PNG` is `include_bytes!`d at compile time, so a decode failure means
// the committed icon is corrupt — a build-time defect, not a runtime condition.
#[allow(clippy::expect_used)]
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
        "Gaming" => Some([139, 92, 246]), // purple — Game Mode, staying out of the way
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

// Infallible: `pixels` is either `base.rgba` cloned or a recolour of it, so its length
// always matches `base.width * base.height * 4`.
#[allow(clippy::expect_used)]
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
    let portable = portable_ui_mode();

    let status: SharedStatus = Arc::new(Mutex::new(StatusPayload {
        status: "Connecting".to_string(),
        error: Some("Connecting to Eir service…".to_string()),
        ..Default::default()
    }));
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiRequest>(16);
    let command_waiters: CommandWaiters = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let connected: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let connected_for_pipe = connected.clone();
    let waiters_for_pipe = command_waiters.clone();
    let autostart_io = Arc::new(Mutex::new(()));
    let autostart_for_setup = autostart_io.clone();

    let status_for_loop = status.clone();

    // Game Mode auto-detector inputs (cloned before the Builder consumes the originals).
    let status_for_detect = status.clone();
    let connected_for_detect = connected.clone();
    let ui_cmd_tx_for_detect = ui_cmd_tx.clone();
    let ui_cmd_state = UiCmdTx {
        sender: ui_cmd_tx,
        connected: connected.clone(),
        status: status.clone(),
        waiters: command_waiters,
        next_id: AtomicU64::new(request_id_seed()),
    };

    let builder = tauri::Builder::default();
    // Installed Eir remains single-instance. Each self-extracted portable has its own
    // private service/pipe and must not collide with an installed tray process.
    let builder = if installed_integrations_enabled(portable) {
        builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if argv.iter().any(|a| a == AUTOSTART_ARG) {
                return;
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
    } else {
        builder
    };

    builder
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Eir")
                .arg(AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(status)
        .manage(ui_cmd_state)
        .manage(ConnState(connected))
        .manage(AutostartIo(autostart_io))
        .manage(RuntimeMode { portable })
        .manage(AskAttachments(std::sync::Mutex::new(Vec::new())))
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), PageLoadEvent::Finished)
                && webview.label() == "main"
                && !launched_hidden()
            {
                let window = webview.window();
                let _ = window.show();
                let _ = window.set_focus();
            }
        })
        .setup(move |app| {
            let icon_base = Arc::new(decode_icon());

            if installed_integrations_enabled(portable) {
                // Keep filesystem and registry I/O off the UI thread. The shared lock
                // preserves migration → read → apply ordering against a fast user save.
                let autostart_handle = app.handle().clone();
                tauri::async_runtime::spawn_blocking(move || {
                    let _guard = autostart_for_setup
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    if let Err(e) = migrate_app_folder(&autostart_handle) {
                        warn!("Could not migrate app folder: {e}");
                    }
                    sync_autostart_on_startup(&autostart_handle);
                });

                // The updater installs NSIS and therefore belongs only to installed Eir.
                spawn_update_checker(app.handle().clone());
            }

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
                    let tx = app.state::<UiCmdTx>().sender.clone();
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
                                    let _ = tx.send(UiMsg::TogglePause.into()).await;
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

            // Background: pipe client + tray colour sync
            let status_pipe = status_for_loop.clone();
            tauri::async_runtime::spawn(async move {
                pipe_client::run(status_pipe, ui_cmd_rx, connected_for_pipe, waiters_for_pipe)
                    .await;
            });

            // Background: Game Mode fullscreen auto-detector (reports over the pipe).
            tauri::async_runtime::spawn(game_detect::run(
                status_for_detect,
                ui_cmd_tx_for_detect,
                connected_for_detect,
            ));

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
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            decide_approval,
            toggle_pause,
            undo_registry,
            ask_eir,
            clear_ask,
            scan_disk,
            clean_disk_entry,
            scan_startup,
            set_startup_entry,
            update_settings,
            test_provider,
            clear_problems,
            clear_executions,
            refresh_status,
            set_gaming,
            add_ask_attachments,
            clear_ask_attachments,
            remove_ask_attachment,
            run_updates_now,
            retry_app_update,
            clear_update_history,
            set_updater_settings,
            set_app_ignore,
            set_app_note,
            set_learned_fact,
            set_advisor_settings,
            get_app_version,
            get_service_version,
            is_portable,
            get_service_state,
            install_service,
            check_updates_now,
            get_autostart_enabled,
            set_autostart_enabled,
            provider_models::list_provider_models,
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

    #[test]
    fn portable_ui_isolated_from_installed_integrations() {
        assert!(portable_ui_mode_at(Some(std::ffi::OsStr::new("1"))));
        assert!(!portable_ui_mode_at(None));
        assert!(!installed_integrations_enabled(true));
        assert!(installed_integrations_enabled(false));
        assert!(
            service_install_path_at(std::path::Path::new(r"C:\Temp\Eir\eir.exe"), true).is_err()
        );
        assert_eq!(runtime_service_state(true, true), Some("running"));
        assert_eq!(runtime_service_state(true, false), Some("stopped"));
        assert_eq!(runtime_service_state(false, true), None);
    }

    #[test]
    fn portable_frontend_hides_privileged_update_ui_and_labels_its_runtime() {
        let html = include_str!("../../ui/index.html");
        let javascript = include_str!("../../ui/main.js");

        for id in ["nav-updates", "card-updater", "about-description"] {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "missing portable-aware element {id}"
            );
        }
        for marker in [
            "navUpdates.hidden = portable",
            "updaterCard.hidden = portable",
            "Portable service: ",
            "aboutDescription.textContent = portable",
        ] {
            assert!(
                javascript.contains(marker),
                "portable UI policy is missing {marker}"
            );
        }
    }

    #[test]
    fn a_failed_status_poll_is_shown_and_announced() {
        // A swallowed get_status failure left "Initializing…" on screen forever with
        // every action disabled and no announced reason. The catch must state it in
        // the role="status" line and keep the actions marked unavailable.
        let javascript = include_str!("../../ui/main.js");
        let (_, after) = javascript
            .split_once("status = await invoke('get_status');")
            .expect("the status poll is still here");
        let catch_block = &after[..after
            .find("lastStatus = status;")
            .expect("the poll's success path is still here")];
        assert!(catch_block.contains("get_status failed"), "wrong block");
        for marker in [
            "'Service unavailable'",
            "setServiceActionsDisconnected(true)",
        ] {
            assert!(
                catch_block.contains(marker),
                "a failed status poll must surface {marker}"
            );
        }
    }

    #[test]
    fn arming_an_irreversible_approval_is_announced() {
        // The two-click safeguard communicated only by swapping the focused button's
        // label, which assistive tech does not reliably re-announce. It must also go
        // through the aria-live toast region.
        let html = include_str!("../../ui/index.html");
        let javascript = include_str!("../../ui/main.js");
        assert!(
            html.contains("id=\"toast-wrap\" aria-live=\"polite\""),
            "toasts are the announced channel"
        );
        let (_, after) = javascript
            .split_once("btn.textContent = 'Click again to confirm — cannot be undone';")
            .expect("the arming branch is still here");
        let arming = &after[..after
            .find("btn._confirmTimer")
            .expect("the 6s disarm timer")];
        assert!(
            arming.contains("toast(") && arming.contains("cannot be undone"),
            "arming an irreversible approval must be announced"
        );
    }

    #[test]
    fn updater_interval_frontend_matches_service_seconds_contract() {
        // The service clamps schedule_interval_secs to 300 … 365 days. The frontend
        // showed whole hours with a 1-hour floor, so a valid 5-minute schedule was
        // displayed as 1 and written back as 3600 by any unrelated updater save.
        const MIN: u64 = 300;
        const MAX: u64 = 365 * 24 * 3600;
        let html = include_str!("../../ui/index.html");
        let javascript = include_str!("../../ui/main.js");
        assert!(html.contains("Check interval (seconds)"));
        assert!(html.contains(&format!(
            "id=\"set-upd-interval\" type=\"number\" min=\"{MIN}\" max=\"{MAX}\""
        )));
        assert!(javascript.contains(&format!(
            "Math.min({MAX}, Math.max({MIN}, s.schedule_interval_secs || 86400))"
        )));
        assert!(javascript.contains(&format!(
            "schedule_interval_secs: numVal('set-upd-interval', 86400, {MIN}, {MAX})"
        )));
    }

    #[test]
    fn destructive_clear_actions_require_confirmation() {
        // Each Clear button permanently deletes history; one stray activation used to
        // do it with no chance to cancel. Confirmation must come BEFORE the invoke.
        let javascript = include_str!("../../ui/main.js");
        for (id, command) in [
            ("clear-ask", "invoke('clear_ask')"),
            ("clear-activity", "invoke('clear_problems')"),
            ("clear-updates", "invoke('clear_update_history')"),
        ] {
            let handler = javascript
                .split_once(&format!(
                    "document.getElementById('{id}').addEventListener('click'"
                ))
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| panic!("{id} handler is still here"));
            let body = &handler[..handler
                .find(command)
                .unwrap_or_else(|| panic!("{id} still calls {command}"))];
            assert!(
                body.contains("window.confirm("),
                "{id} deletes history without confirming first"
            );
        }
    }

    #[test]
    fn service_trigger_buttons_show_progress_synchronously() {
        // "Update now" left the button live and unchanged until the next 2s poll, so a
        // second activation issued a duplicate run. It must arm itself on click like
        // the scan buttons, and refresh so the poll reconciles the real state.
        let javascript = include_str!("../../ui/main.js");
        for (id, command) in [
            ("upd-now", "invoke('run_updates_now')"),
            ("disk-scan", "invoke('scan_disk')"),
        ] {
            let handler = javascript
                .split_once(&format!(
                    "document.getElementById('{id}').addEventListener('click'"
                ))
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| panic!("{id} handler is still here"));
            let before_invoke = &handler[..handler
                .find(command)
                .unwrap_or_else(|| panic!("{id} still calls {command}"))];
            assert!(
                before_invoke.contains("btn.disabled = true"),
                "{id} must disable itself before invoking"
            );
        }
    }

    #[test]
    fn rejected_command_result_is_an_error() {
        assert_eq!(
            command_result(CommandResult {
                request_id: 1,
                ok: false,
                message: "paused".to_string(),
            }),
            Err("paused".to_string())
        );
    }

    #[test]
    fn ask_attachment_batches_do_not_consume_new_picks() {
        let attachment = |name: &str| eir_proto::AskAttachment {
            name: name.to_string(),
            kind: "text".to_string(),
            content: name.to_string(),
            media_type: String::new(),
        };
        let atts = AskAttachments(std::sync::Mutex::new(vec![attachment("sent.txt")]));

        let sent = take_ask_attachments(&atts).expect("take submitted batch");
        atts.0
            .lock()
            .expect("lock pending attachments")
            .push(attachment("next.txt"));
        restore_ask_attachments(&atts, sent).expect("restore failed batch");

        let names = atts
            .0
            .lock()
            .expect("lock restored attachments")
            .iter()
            .map(|attachment| attachment.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, ["sent.txt", "next.txt"]);
    }

    #[test]
    fn service_install_command_is_safe_and_propagates_exit() {
        let hostile = Path::new(r#"C:\Eir`"; throw 'injected'; #\eir-svc.exe"#);
        let command = service_install_command(hostile);
        let std = command.as_std();
        let args = std
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!args.contains("throw 'injected'"));
        assert!(args.contains(".ExitCode"));
        assert_eq!(
            std.get_envs()
                .find(|(key, _)| *key == "EIR_SERVICE_EXE")
                .and_then(|(_, value)| value),
            Some(hostile.as_os_str())
        );
    }
}
