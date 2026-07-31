mod ai;
mod ask;
mod audit;
mod config;
mod digest;
mod disk_scan;
mod executor;
mod explain;
mod feedback;
mod game_mode;
mod learn;
mod models;
mod pipe_server;
mod policy;
mod safety;
mod service_install;
mod session;
mod signals;
mod startup_scan;
mod updater;

use eir_proto::{
    AdvisorStatus, ApprovalInfo, ExecutionSummary, LearnedFactView, ProblemSummary, StatusPayload,
    UiMsg, UiSettings, UpdaterStatus, UsageSummary,
};
use models::{
    CallUsage, ClaudeDecision, ExecutionResult, FixAction, PendingApproval, SignalSnapshot,
    SystemState,
};
use sqlx::SqlitePool;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
        ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_NAME: &str = "EirSvc";
const SERVICE_DISPLAY: &str = "Eir System Monitor";

// ── Windows service boilerplate ───────────────────────────────────────────────

define_windows_service!(ffi_service_main, svc_main);

fn svc_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(e) = run_service() {
        eprintln!("Service run error: {e:?}");
    }
}

// The Tokio runtime build below is fail-fast on purpose: without a runtime there is no
// service to degrade into, and SCM reports the failure to the event log.
#[allow(clippy::expect_used)]
fn run_service() -> windows_service::Result<()> {
    service_install::validate_current_binary().map_err(|error| {
        windows_service::Error::Winapi(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing insecure service registration: {error:#}"),
        ))
    })?;
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_signal = shutdown.clone();

    // The event handler needs the status handle to report StopPending, but `register`
    // returns it — so hand it in through a shared cell populated right after registering.
    let status_slot: std::sync::Arc<
        std::sync::OnceLock<service_control_handler::ServiceStatusHandle>,
    > = std::sync::Arc::new(std::sync::OnceLock::new());
    let status_slot_h = status_slot.clone();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                shutdown_signal.store(true, std::sync::atomic::Ordering::SeqCst);
                // Tell SCM we're stopping and to allow time for the executor drain, so it
                // doesn't force-kill the process (aborting an in-flight fix) mid-drain.
                if let Some(h) = status_slot_h.get() {
                    let _ = h.set_service_status(ServiceStatus {
                        service_type: ServiceType::OWN_PROCESS,
                        current_state: ServiceState::StopPending,
                        controls_accepted: ServiceControlAccept::empty(),
                        exit_code: ServiceExitCode::Win32(0),
                        checkpoint: 0,
                        wait_hint: std::time::Duration::from_secs(35),
                        process_id: None,
                    });
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    let _ = status_slot.set(status_handle);
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime");

    rt.block_on(eir_main(
        async move {
            // Poll the atomic flag until Stop/Shutdown is received
            loop {
                if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        },
        None,
    ));

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

// Fail-fast CLI path: `eir-svc install` is run interactively by an admin (or the
// installer), so a panic with a readable reason is the intended outcome — there is no
// service to keep alive yet and no caller to hand an error to.
#[allow(clippy::expect_used)]
fn install_service() {
    service_install::install_or_update(SERVICE_NAME, SERVICE_DISPLAY)
        .expect("Failed to securely install or update service");
    println!("{SERVICE_NAME} installed and started.");
    println!("Stop it with: sc stop {SERVICE_NAME}");
}

// Fail-fast CLI path — see `install_service`.
#[allow(clippy::expect_used)]
fn uninstall_service() {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)
        .expect("Failed to connect to service manager (run as Administrator)");
    let svc = manager
        .open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::STOP)
        .expect("Failed to open service (is it installed?)");
    let _ = svc.stop();
    svc.delete().expect("Failed to delete service");
    println!("{SERVICE_NAME} uninstalled.");
}

fn validated_portable_sentinel_at(sentinel: &Path, executable: &Path) -> Option<PathBuf> {
    if sentinel.file_name()?.to_string_lossy() != "eir-portable.running" {
        return None;
    }
    let sentinel = executor::logs::checked_local_path(sentinel).ok()??;
    let executable = executor::logs::checked_local_path(executable).ok()??;
    sentinel
        .parent()?
        .to_string_lossy()
        .eq_ignore_ascii_case(&executable.parent()?.to_string_lossy())
        .then_some(sentinel)
}

fn validated_portable_sentinel(value: &str) -> Option<PathBuf> {
    validated_portable_sentinel_at(Path::new(value), &std::env::current_exe().ok()?)
}

fn validated_portable_pipe_name(value: &str) -> Option<String> {
    const PREFIX: &str = r"\\.\pipe\EirSvcPortable-";
    let nonce = value.strip_prefix(PREFIX)?;
    (nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_string())
}

fn validated_portable_state_root_at(value: &Path, local_app_data: &Path) -> Option<PathBuf> {
    let value = executor::logs::checked_local_path(value).ok()??;
    let expected =
        executor::logs::checked_local_path(&local_app_data.join("EirPortable")).ok()??;
    (value.is_dir()
        && value
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy()))
    .then_some(value)
}

fn validated_portable_state_root(value: &str) -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    validated_portable_state_root_at(Path::new(value), Path::new(&local_app_data))
}

// Standalone-mode runtime build is fail-fast — see `run_service`.
#[allow(clippy::expect_used)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("install") => install_service(),
        Some("uninstall") => uninstall_service(),
        Some("portable") => {
            if !pipe_server::current_process_portable_allowed() {
                eprintln!("portable mode refuses split-token elevation or LocalSystem execution");
                std::process::exit(2);
            }
            let Some(sentinel) = args
                .get(2)
                .and_then(|value| validated_portable_sentinel(value))
            else {
                eprintln!("portable mode requires the trusted sibling sentinel");
                std::process::exit(2);
            };
            let Some(pipe_name) = args
                .get(3)
                .and_then(|value| validated_portable_pipe_name(value))
            else {
                eprintln!("portable mode requires a private pipe name");
                std::process::exit(2);
            };
            let Some(state_root) = args
                .get(4)
                .and_then(|value| validated_portable_state_root(value))
            else {
                eprintln!("portable mode requires its trusted persistent state directory");
                std::process::exit(2);
            };
            if let Err(error) = config::set_runtime_root(state_root) {
                eprintln!("portable mode could not configure its state directory: {error}");
                std::process::exit(2);
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Tokio runtime");
            rt.block_on(eir_main(
                async move {
                    while tokio::fs::symlink_metadata(&sentinel).await.is_ok() {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                },
                Some(pipe_name),
            ));
        }
        _ => {
            // Try SCM dispatch; on failure run standalone (development / debugging).
            if service_dispatcher::start(SERVICE_NAME, ffi_service_main).is_err() {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Tokio runtime");
                // Standalone: run until Ctrl-C
                rt.block_on(eir_main(
                    async {
                        let _ = tokio::signal::ctrl_c().await;
                    },
                    None,
                ));
            }
        }
    }
}

#[cfg(test)]
mod portable_mode_tests {
    use super::*;

    #[test]
    fn portable_sentinel_must_be_a_regular_sibling_of_the_service() {
        let root =
            std::env::temp_dir().join(format!("eir-portable-sentinel-{}", std::process::id()));
        let other = root.join("other");
        let service = root.join("eir-svc.exe");
        let sentinel = root.join("eir-portable.running");
        let outside = other.join("eir-portable.running");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(&service, b"test").unwrap();
        std::fs::write(&sentinel, b"running").unwrap();
        std::fs::write(&outside, b"running").unwrap();

        assert_eq!(
            validated_portable_sentinel_at(&sentinel, &service),
            std::fs::canonicalize(&sentinel).ok()
        );
        assert_eq!(validated_portable_sentinel_at(&outside, &service), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn portable_pipe_name_is_random_and_bounded() {
        assert!(validated_portable_pipe_name(
            r"\\.\pipe\EirSvcPortable-0123456789abcdef0123456789abcdef"
        )
        .is_some());
        assert!(validated_portable_pipe_name(eir_proto::PIPE_NAME).is_none());
        assert!(validated_portable_pipe_name(
            r"\\.\pipe\EirSvcPortable-0123456789abcdef0123456789abcde!"
        )
        .is_none());
    }

    #[test]
    fn portable_state_root_is_exact_local_app_data_directory() {
        let root = std::env::temp_dir().join(format!("eir-portable-state-{}", std::process::id()));
        let local_app_data = root.join("AppData").join("Local");
        let expected = local_app_data.join("EirPortable");
        let outside = local_app_data.join("Other");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&expected).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert_eq!(
            validated_portable_state_root_at(&expected, &local_app_data),
            std::fs::canonicalize(&expected).ok()
        );
        assert_eq!(
            validated_portable_state_root_at(&outside, &local_app_data),
            None
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn portable_settings_never_invoke_the_installed_service_restart() {
        assert!(should_restart_service_after_settings(true, false));
        assert!(!should_restart_service_after_settings(true, true));
        assert!(!should_restart_service_after_settings(false, false));
    }
}

// ── Service state ─────────────────────────────────────────────────────────────

struct SvcState {
    paused: bool,
    cpu: f32,
    memory: f32,
    disk: f32,
    failed_services: Vec<String>,
    signals_at: i64,
    signal_errors: Vec<String>,
    last_analysis: String,
    /// Unix seconds the last completed AI analysis finished (0 = none yet this run).
    last_analysis_unix: i64,
    recent_problems: VecDeque<ProblemSummary>,
    recent_executions: VecDeque<ExecutionSummary>,
    /// Actions awaiting the user's decision. Mirrored to the audit DB so the queue
    /// survives restarts; the user can approve or reject any item at any time.
    pending: Vec<PendingApproval>,
    status: String,
    error: Option<String>,
    usage: Option<UsageSummary>,
    settings: Option<UiSettings>,
    /// Autonomous-updater status broadcast to the UI.
    updater: UpdaterStatus,
    /// True while an update cycle is in flight (prevents overlapping cycles).
    updater_running: bool,
    /// Debug-labels of fix actions currently queued or executing on the executor
    /// worker. Used to dedupe duplicate enqueues and to reflect "Executing" status
    /// while any action runs off the decision loop.
    in_flight: HashSet<String>,
    /// What Eir has learned about this machine (self-improvement), for the UI card.
    learned_facts: Vec<LearnedFactView>,
    /// Advisor-mode status broadcast to the UI.
    advisor: Option<AdvisorStatus>,
    /// Escalation AI spend accumulated today (reset at the UTC day boundary).
    advisor_spent_today: f64,
    /// Escalations performed today — the hard provider-agnostic backstop.
    advisor_escalations_today: u32,
    /// The UTC date (YYYY-MM-DD) that the advisor day-counters belong to.
    advisor_spend_date: String,
    /// The latest weekly health digest broadcast to the UI (None until generated).
    digest: Option<eir_proto::DigestView>,
    /// Unix seconds the last digest was generated (0 = never), for the weekly gate.
    last_digest_at: i64,
    /// True while a digest generation is in flight (prevents overlap).
    digest_running: bool,
    /// Recent CPU/memory/disk history for the dashboard timeline, refreshed once per
    /// decision tick from `system_state_history`.
    history: Vec<eir_proto::MetricPoint>,
    /// "Ask Eir" free-text Q&A state broadcast to the UI (None until first question).
    ask: Option<eir_proto::AskStatus>,
    /// Recent Ask entries (newest first, capped). Memory-only; lost on restart.
    ask_entries: VecDeque<eir_proto::AskEntry>,
    /// True while an Ask answer is being generated (prevents overlap / spam).
    ask_running: bool,
    /// Unix seconds the last Ask answer finished (0 = never), for the spend guard.
    last_ask_at: i64,
    /// On-demand disk-scan results broadcast to the UI (None until first scan).
    disk_insights: Option<eir_proto::DiskInsightsView>,
    /// True while a disk scan is in flight (prevents overlap).
    disk_scan_running: bool,
    /// Server-side map of the last scan's cleanable entry ids → the safe fix-action each
    /// maps to. A "Clean" click carries only the opaque id; the action is reconstructed
    /// here and routed through the normal policy gate — the wire never carries an action.
    disk_targets: std::collections::HashMap<String, FixAction>,
    /// On-demand startup-scan results broadcast to the UI (None until first scan).
    startup: Option<eir_proto::StartupView>,
    /// Active interactive-user SID that owns `startup` and `startup_targets`.
    startup_owner_sid: Option<String>,
    /// True while a startup scan is in flight (prevents overlap).
    startup_scan_running: bool,
    /// Server-side map of the last startup scan's entry ids → toggle info, so an
    /// enable/disable click can be reconstructed into a `StartupSet` action server-side
    /// (the wire carries only the opaque id and a bool).
    startup_targets: std::collections::HashMap<String, startup_scan::StartupToggle>,
    /// Game Mode is a *lease*, not a latch: the tray re-asserts it on a heartbeat while a
    /// fullscreen game runs, and this holds the unix-secs deadline it's valid until (0 =
    /// off). Gaming is active while `gaming_until > now`, so a crashed/closed tray lets it
    /// auto-expire and Eir resumes full guardian duty — see `is_gaming_at`.
    gaming_until: i64,
    /// The user's explicit Game Mode toggle — a latch (persists until toggled off), unlike
    /// the auto-detector's heartbeat lease. Gaming is active if this is set OR the lease is
    /// live.
    gaming_manual: bool,
}

impl Default for SvcState {
    fn default() -> Self {
        Self {
            paused: false,
            cpu: 0.0,
            memory: 0.0,
            disk: 0.0,
            failed_services: vec![],
            signals_at: 0,
            signal_errors: vec![],
            last_analysis: String::new(),
            last_analysis_unix: 0,
            recent_problems: VecDeque::new(),
            recent_executions: VecDeque::new(),
            pending: Vec::new(),
            status: "Initializing".to_string(),
            error: None,
            usage: None,
            settings: None,
            updater: UpdaterStatus::default(),
            updater_running: false,
            in_flight: HashSet::new(),
            learned_facts: Vec::new(),
            advisor: None,
            advisor_spent_today: 0.0,
            advisor_escalations_today: 0,
            advisor_spend_date: String::new(),
            digest: None,
            last_digest_at: 0,
            digest_running: false,
            history: Vec::new(),
            ask: None,
            ask_entries: VecDeque::new(),
            ask_running: false,
            last_ask_at: 0,
            disk_insights: None,
            disk_scan_running: false,
            disk_targets: std::collections::HashMap::new(),
            startup: None,
            startup_owner_sid: None,
            startup_scan_running: false,
            startup_targets: std::collections::HashMap::new(),
            gaming_until: 0,
            gaming_manual: false,
        }
    }
}

/// Whether Game Mode is active at `now`: the manual latch is set, OR the auto-detector's
/// lease hasn't expired. Pure — the loop's gates/status use `now = Utc::now().timestamp()`;
/// tests pass a fixed `now`.
fn is_gaming_at(st: &SvcState, now: i64) -> bool {
    st.gaming_manual || st.gaming_until > now
}

/// Convenience wrapper reading the real clock, for status/broadcast projection.
fn is_gaming(st: &SvcState) -> bool {
    is_gaming_at(st, chrono::Utc::now().timestamp())
}

/// Apply a `SetGaming` message to the two gaming inputs. Manual is a latch, auto is a
/// lease — but a manual OFF also withdraws the auto lease, so the user's click actually
/// ends Game Mode now instead of silently losing to a still-live lease (the auto
/// detector's next heartbeat may re-assert it — the documented trade-off; turn auto off
/// for full manual control). Pure + testable.
fn apply_set_gaming(st: &mut SvcState, on: bool, manual: bool, now: i64, lease_secs: i64) {
    if manual {
        st.gaming_manual = on;
        if !on {
            st.gaming_until = 0;
        }
    } else {
        st.gaming_until = if on {
            now.saturating_add(lease_secs)
        } else {
            0
        };
    }
}

/// Whether a *scheduled* autonomous update cycle should start now. Pure + testable. Gated
/// on: updater enabled, not paused, not already running, **not in Game Mode**, and the
/// schedule interval having elapsed. The manual `RunUpdatesNow` path bypasses this entirely
/// (an explicit "update now" click is honoured even during a game).
fn updater_due(enabled: bool, interval_secs: i64, last_run: i64, st: &SvcState, now: i64) -> bool {
    enabled
        && !st.paused
        && !st.updater_running
        && !is_gaming_at(st, now)
        && (last_run == 0 || now.saturating_sub(last_run) >= interval_secs)
}

fn failed_update_for_retry(
    apps: &[eir_proto::UpdaterAppRow],
    id: &str,
) -> Option<eir_proto::UpdaterAppRow> {
    updater::config::valid_app_id(id).then(|| {
        apps.iter()
            .find(|app| app.id.eq_ignore_ascii_case(id) && app.state == "failed")
            .cloned()
    })?
}

fn should_restart_service_after_settings(needs_restart: bool, portable_mode: bool) -> bool {
    needs_restart && !portable_mode
}

/// Restart the service to apply new settings: a detached helper waits for this
/// process to stop cleanly, then starts EirSvc (LocalSystem — no UAC).
///
/// The helper *waits* for the service to actually reach STOPPED (up to 60s) before
/// starting it, then retries the start until it reports Running. It does not issue
/// `sc stop`: the caller first flushes its result to the UI, then returns cleanly.
/// Returns false if the helper could not be spawned — the caller must then
/// stay alive (a stop with no helper running means nothing ever starts us
/// again; that failure mode is exactly a "service lost" incident).
///
/// The helper redirects all its streams to `eir-restart.log` next to the exe:
/// by the time a restart has gone wrong this process is gone, so that file is
/// the only evidence of what `sc` said. It waits for the service to reach
/// STOPPED (up to 60s; a missing service also exits the wait), then retries
/// `sc start` every 5s over a 60s window, checking for Running every second —
/// wide enough to ride out a lingering old process or transient SCM refusal.
#[must_use]
fn restart_self() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let log = config::resolve("eir-restart.log");
    let log_q = crate::executor::powershell::ps_single_quote(&log.to_string_lossy());
    let script = format!(
        "& {{ Get-Date -Format o; \
         $d=(Get-Date).AddSeconds(60); \
         while((Get-Date) -lt $d){{ \
             $s=(Get-Service EirSvc -ErrorAction SilentlyContinue).Status; \
             if($null -eq $s -or $s -eq 'Stopped'){{break}}; \
             Start-Sleep -Milliseconds 500 }}; \
         for($i=0;$i -lt 60;$i++){{ \
             if((Get-Service EirSvc -ErrorAction SilentlyContinue).Status -eq 'Running'){{break}}; \
             if($i % 5 -eq 0){{ sc.exe start EirSvc }}; \
             Start-Sleep -Seconds 1 }}; \
         Get-Date -Format o; \
         Get-Service EirSvc -ErrorAction SilentlyContinue | Format-List Name,Status,StartType }} \
         *> {log_q}",
    );
    match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
    {
        Ok(child) => {
            info!(helper_pid = child.id(), "Restart helper spawned");
            true
        }
        Err(e) => {
            error!("Failed to spawn restart helper: {e}");
            false
        }
    }
}

fn startup_owner_matches(owner_sid: Option<&str>, active_sid: Option<&str>) -> bool {
    matches!((owner_sid, active_sid), (Some(owner), Some(active)) if owner.eq_ignore_ascii_case(active))
}

fn optional_sids_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

fn watch_roots_need_clearing(owner_sid: Option<&str>, active_sid: Option<&str>) -> bool {
    owner_sid.is_some() && !optional_sids_match(owner_sid, active_sid)
}

fn discover_watch_dirs_for_stable_user(extra: &[String]) -> Option<(String, Vec<PathBuf>)> {
    let before = executor::startup::active_user_sid().ok()?;
    let directories = signals::file_watch::discover_watch_dirs(extra)?;
    let after = executor::startup::active_user_sid().ok()?;
    before
        .eq_ignore_ascii_case(&after)
        .then_some((before, directories))
}

async fn abort_task<T>(handle: tokio::task::JoinHandle<T>) {
    handle.abort();
    let _ = handle.await;
}

fn spawn_watch_session_reconciler(
    mut owner_sid: Option<String>,
    extra: Vec<String>,
    updates: signals::file_watch::DirUpdateSender,
) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let current =
                match tokio::task::spawn_blocking(|| executor::startup::active_user_sid().ok())
                    .await
                {
                    Ok(sid) => sid,
                    Err(error) => {
                        warn!("Active-user watcher check failed: {error}");
                        continue;
                    }
                };
            if optional_sids_match(owner_sid.as_deref(), current.as_deref()) {
                continue;
            }
            // Stop observing the previous user's roots before discovery. Discovery
            // may transiently fail during sign-in; retaining old SYSTEM watches
            // during that retry window would cross the session boundary.
            if watch_roots_need_clearing(owner_sid.as_deref(), current.as_deref()) {
                if updates.send(Vec::new()).is_err() {
                    break;
                }
                owner_sid = None;
            }
            let Some(expected_sid) = current else {
                continue;
            };
            let extra = extra.clone();
            let discovery =
                tokio::task::spawn_blocking(move || discover_watch_dirs_for_stable_user(&extra))
                    .await;
            let Ok(Some((discovered_sid, directories))) = discovery else {
                continue;
            };
            if !expected_sid.eq_ignore_ascii_case(&discovered_sid) {
                continue;
            }
            if updates.send(directories).is_err() {
                break;
            }
            owner_sid = Some(discovered_sid);
        }
    });
}

fn clear_stale_startup_cache(st: &mut SvcState, active_sid: Option<&str>) -> bool {
    if st.startup_owner_sid.is_none()
        || startup_owner_matches(st.startup_owner_sid.as_deref(), active_sid)
    {
        return false;
    }
    st.startup = None;
    st.startup_owner_sid = None;
    st.startup_scan_running = false;
    st.startup_targets.clear();
    true
}

fn build_status(st: &SvcState) -> StatusPayload {
    let active_sid = executor::startup::active_user_sid().ok();
    let startup = startup_owner_matches(st.startup_owner_sid.as_deref(), active_sid.as_deref())
        .then(|| st.startup.clone())
        .flatten();
    StatusPayload {
        protocol_version: eir_proto::PROTOCOL_VERSION,
        capabilities: vec![
            eir_proto::CAP_COMMAND_RESULTS.to_string(),
            eir_proto::CAP_PROVIDER_TEST.to_string(),
            eir_proto::CAP_TARGETED_UPDATE_RETRY.to_string(),
        ],
        status: st.status.clone(),
        paused: st.paused,
        cpu: st.cpu,
        memory: st.memory,
        disk: st.disk,
        failed_services: st.failed_services.clone(),
        last_analysis: st.last_analysis.clone(),
        last_analysis_at: st.last_analysis_unix,
        recent_problems: st.recent_problems.iter().cloned().collect(),
        recent_executions: st.recent_executions.iter().cloned().collect(),
        pending_approvals: st.pending.iter().map(|p| p.info.clone()).collect(),
        error: st.error.clone(),
        usage: st.usage.clone(),
        settings: st.settings.clone(),
        updater: Some(st.updater.clone()),
        advisor: st.advisor.clone(),
        learned_facts: st.learned_facts.clone(),
        digest: st.digest.clone(),
        history: st.history.clone(),
        ask: st.ask.clone(),
        disk_insights: st.disk_insights.clone(),
        startup,
        gaming: is_gaming(st),
        svc_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        signals_at: st.signals_at,
        signal_errors: st.signal_errors.clone(),
    }
}

/// Rebuild the broadcast "Ask Eir" view from the running flag, entry history, and an
/// optional error to surface. The error persists in `st.ask` until the next ask event
/// replaces it.
fn refresh_ask(st: &mut SvcState, error: Option<String>) {
    st.ask = Some(eir_proto::AskStatus {
        running: st.ask_running,
        error,
        entries: st.ask_entries.iter().cloned().collect(),
    });
}

/// Hard backstop on escalations per UTC day. Applied regardless of provider or
/// whether the provider reports cost, so a runaway agent cannot exceed this count.
const MAX_ESCALATIONS_PER_DAY: u32 = 24;

/// Decide whether the advisor should re-analyse at a higher tier, and why. Pure.
/// Returns `Some(reason)` to escalate. Bounded: it fires only when advisor mode is on,
/// a deeper tier is configured, the per-day count cap is not spent, AND the agent
/// flagged ambiguity or the best confidence is below the threshold.
fn should_escalate(
    decision: &models::ClaudeDecision,
    cfg: &config::AdvisorConfig,
    _spent_today: f64,
    escalations_today: u32,
) -> Option<&'static str> {
    if !cfg.enabled {
        return None;
    }
    // A deeper pass needs at least one lever (a stronger model or a higher effort).
    if cfg.escalation_model.trim().is_empty() && cfg.escalation_effort.trim().is_empty() {
        return None;
    }
    if escalations_today >= MAX_ESCALATIONS_PER_DAY {
        return None;
    }
    if decision.needs_deeper_analysis {
        return Some("the agent flagged the signals as ambiguous");
    }
    let max_conf = decision
        .problems
        .iter()
        .map(|p| p.confidence)
        .fold(0.0_f32, f32::max);
    if !decision.problems.is_empty() && max_conf < cfg.low_confidence_threshold {
        return Some("confidence was low");
    }
    None
}

/// Spawn one update cycle on a detached task so the multi-minute run never blocks the
/// monitoring loop. The cycle builds its own AI client from the current config and
/// reports the finished [`updater::orchestrator::CycleSummary`] back over `done_tx`.
fn spawn_update_cycle(
    cfg: &config::Config,
    db: &SqlitePool,
    done_tx: &tokio::sync::mpsc::Sender<updater::orchestrator::CycleSummary>,
    progress_tx: &updater::orchestrator::ProgressTx,
    retry: Option<eir_proto::UpdaterAppRow>,
) {
    let api = cfg.api.clone();
    let updater_cfg = cfg.updater.clone();
    let model = cfg.api.update_check_model.clone();
    let pool = db.clone();
    let tx = done_tx.clone();
    let progress = progress_tx.clone();
    let cycle_id = chrono::Utc::now().timestamp();
    // Hard ceiling on a whole cycle. Per-command timeouts already bound each external
    // call, so a healthy run finishes far inside this; it is the last-resort backstop
    // that guarantees `updater_running` is always released even if something
    // unforeseen wedges.
    const CYCLE_MAX: Duration = Duration::from_secs(60 * 60);
    tokio::spawn(async move {
        let targeted = retry.is_some();
        // Run the cycle in an inner task so a panic surfaces as a JoinError (not a
        // silent abort) AND a watchdog can stop a hang — either way a summary is sent
        // and `updater_running` is released, so the updater can never latch "running"
        // forever and wedge every future cycle.
        let mut inner = tokio::spawn(async move {
            let ai = ai::client::AiClient::new(&api).await.ok();
            let ctx = updater::orchestrator::EngineCtx {
                ai: ai.as_ref(),
                config: &updater_cfg,
                model_override: &model,
            };
            match retry {
                Some(prior) => {
                    updater::orchestrator::run_retry(&pool, &ctx, cycle_id, &progress, prior).await
                }
                None => updater::orchestrator::run_cycle(&pool, &ctx, cycle_id, &progress).await,
            }
        });
        let summary = match tokio::time::timeout(CYCLE_MAX, &mut inner).await {
            Ok(Ok(summary)) => summary,
            Ok(Err(_join)) => updater::orchestrator::CycleSummary {
                results: Vec::new(),
                notes: vec!["update cycle aborted unexpectedly".to_string()],
                cost_usd: 0.0,
                completed_at: chrono::Utc::now().timestamp(),
                clean_at: 0,
                targeted,
                learned_guidance: None,
            },
            Err(_elapsed) => {
                abort_task(inner).await;
                updater::orchestrator::CycleSummary {
                    results: Vec::new(),
                    notes: vec![format!(
                        "update cycle exceeded {}m and was stopped",
                        CYCLE_MAX.as_secs() / 60
                    )],
                    cost_usd: 0.0,
                    completed_at: chrono::Utc::now().timestamp(),
                    clean_at: 0,
                    targeted,
                    learned_guidance: None,
                }
            }
        };
        let _ = tx.send(summary).await;
    });
}

/// The status to settle on when not mid-cycle: paused beats everything, then an
/// outstanding approval, then an action still executing off the loop, otherwise active.
fn resting_status(st: &SvcState) -> String {
    if st.paused {
        "Paused"
    } else if is_gaming(st) {
        // Below Paused (an explicit user pause outranks auto Game Mode), above the rest.
        "Gaming"
    } else if !st.pending.is_empty() {
        "PendingApproval"
    } else if !st.in_flight.is_empty() {
        "Executing"
    } else {
        "Active"
    }
    .to_string()
}

/// A fix action handed to the executor worker, with everything needed to run it
/// and to record the outcome back on the decision loop afterwards.
struct ExecJob {
    action: FixAction,
    decision_id: i64,
    baseline: SystemState,
    /// Semantic identity used for in-flight deduplication.
    key: String,
    /// `format!("{action:?}")` — the label shown in the activity feed.
    label: String,
    diagnosis: String,
    confidence: f32,
    /// Why it ran (e.g. "approved by user"); None for an autonomous fix.
    reason: Option<String>,
    /// Persisted approved row retained until the execution is durably logged.
    approval_id: Option<u64>,
}

/// The result of one executor job, folded back into `SvcState` on the decision loop.
struct ExecOutcome {
    key: String,
    label: String,
    /// `result.action` (the executed action's Debug form) for the execution log entry.
    exec_action: String,
    success: bool,
    output: String,
    diagnosis: String,
    confidence: f32,
    reason: Option<String>,
    /// For a successful registry reset, the persisted undo-record id so the UI can
    /// offer a one-click revert. None for everything else.
    undo_id: Option<i64>,
    /// Set to an undo-record id when a user-triggered undo SUCCEEDED, so the loop can
    /// clear that id from the original activity row (its "Undo" button disappears).
    cleared_undo_id: Option<i64>,
    /// Reload the durable approval queue after an execution failure requeued its row.
    refresh_pending: bool,
}

/// Slow AI work for one decision-loop pass, run off-loop (mirrors ExecJob/ExecOutcome
/// and spawn_update_cycle's hardening) so ui_rx keeps being polled while an analysis
/// (and its optional advisor escalation) is in flight.
struct AnalysisSuccess {
    snapshot: SignalSnapshot,
    fingerprint: Option<String>,
    decision: ClaudeDecision,
    usage: Vec<CallUsage>,
    advisor: AdvisorStatus,
    advisor_spent_today: f64,
    advisor_escalations_today: u32,
    /// Marginal cost of THIS analysis's escalation (0.0 if it didn't escalate), so the
    /// completion arm can attribute just this run's spend if the UTC day rolled over
    /// while the analysis was in flight.
    escalation_cost_usd: f64,
}

/// Per-job backstop timeout for the executor worker. Defaults to 10 minutes, but the
/// system-file repairs (`sfc /scannow`, DISM RestoreHealth) are legitimately long and
/// get a much larger cap so the backstop doesn't kill a healthy repair mid-run.
fn exec_timeout(action: &FixAction) -> Duration {
    match action {
        FixAction::SfcScan | FixAction::DismRestoreHealth => executor::repair::REPAIR_TIMEOUT,
        _ => Duration::from_secs(10 * 60),
    }
}

/// Spawn the single executor worker. It serialises fix-action execution off the
/// decision loop: each job runs `executor::execute` (panic-isolated), then durably
/// commits its audit/approval transition and writes feedback before reporting an
/// [`ExecOutcome`] back over `done_tx` for the loop to fold into `SvcState`. Because
/// execution no longer blocks the loop, UI
/// commands and status updates stay responsive however long an action takes.
fn spawn_executor(
    db: &SqlitePool,
    mut job_rx: tokio::sync::mpsc::UnboundedReceiver<ExecJob>,
    done_tx: tokio::sync::mpsc::UnboundedSender<ExecOutcome>,
) -> tokio::task::JoinHandle<()> {
    let db = db.clone();
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let ExecJob {
                action,
                decision_id,
                baseline,
                key,
                label,
                diagnosis,
                confidence,
                reason,
                approval_id,
            } = job;
            // Per-job backstop timeout. Most actions carry their own timeout (powershell
            // DEFAULT_TIMEOUT, driver/security calls, etc.); this bounds the queue so one
            // wedged action can't latch every later action on "Executing". SFC/DISM are
            // legitimately long, so they get a longer cap than the default.
            let exec_max = exec_timeout(&action);
            // Isolate only the executor itself. Persistence happens afterwards in this
            // worker so panic/timeout outcomes go through the same durable transition.
            let action_to_execute = action.clone();
            let mut handle =
                tokio::spawn(async move { executor::execute(&action_to_execute).await });
            let (mut result, synthetic_failure) =
                match tokio::time::timeout(exec_max, &mut handle).await {
                    Ok(Ok(result)) => (result, false),
                    Ok(Err(_join)) => (
                        ExecutionResult {
                            action: label.clone(),
                            success: false,
                            output: "execution task panicked".to_string(),
                            undo: None,
                        },
                        true,
                    ),
                    Err(_elapsed) => {
                        handle.abort();
                        let _ = handle.await;
                        (
                            ExecutionResult {
                                action: label.clone(),
                                success: false,
                                output: format!(
                                    "execution exceeded {}m and was abandoned",
                                    exec_max.as_secs() / 60
                                ),
                                undo: None,
                            },
                            true,
                        )
                    }
                };

            let requeue_approval = synthetic_failure && approval_id.is_some();
            let mut undo_id = None;
            let mut refresh_pending = false;
            let mut persisted = None;
            match audit::persist_execution(
                &db,
                decision_id,
                &action,
                &result,
                approval_id,
                requeue_approval,
            )
            .await
            {
                Ok(commit) => {
                    refresh_pending = requeue_approval;
                    persisted = Some(commit);
                }
                Err(commit_error)
                    if result.success && matches!(action, FixAction::RegistryReset { .. }) =>
                {
                    // The reset is only safe while its exact undo is durable. If that
                    // atomic commit fails, restore the live value immediately. A
                    // successful rollback is then logged as a failure and an approved
                    // action is returned to the queue.
                    error!("Failed to persist registry reset and undo: {commit_error}");
                    if let Some(undo) = result.undo.clone() {
                        match executor::registry::restore_value(&undo).await {
                            Ok(rollback_output) => {
                                result = ExecutionResult {
                                    action: result.action,
                                    success: false,
                                    output: format!(
                                        "Registry reset was rolled back because its undo could not be stored ({commit_error}): {rollback_output}"
                                    ),
                                    undo: None,
                                };
                                let fallback_requeue = approval_id.is_some();
                                match audit::persist_execution(
                                    &db,
                                    decision_id,
                                    &action,
                                    &result,
                                    approval_id,
                                    fallback_requeue,
                                )
                                .await
                                {
                                    Ok(commit) => {
                                        refresh_pending = fallback_requeue;
                                        persisted = Some(commit);
                                    }
                                    Err(e) => {
                                        error!("Failed to log rolled-back registry reset: {e}");
                                        result.output.push_str(&format!(
                                            "; the rollback audit could not be stored: {e}"
                                        ));
                                    }
                                }
                            }
                            Err(rollback_error) => {
                                result.success = false;
                                result.undo = None;
                                result.output = format!(
                                    "Registry reset ran, but its undo could not be stored ({commit_error}) and automatic rollback failed ({rollback_error}); the approved action was retained for recovery"
                                );
                                error!("{}", result.output);
                            }
                        }
                    } else {
                        result.success = false;
                        result.output = format!(
                            "Registry reset ran, but no exact undo was available ({commit_error}); the approved action was retained for recovery"
                        );
                        error!("{}", result.output);
                    }
                }
                Err(e) => {
                    error!("Failed to durably record execution: {e}");
                    result.output.push_str(&format!(
                        "; execution audit and approval transition failed: {e}"
                    ));
                }
            }

            if let Some(commit) = persisted {
                undo_id = commit.undo_id;
                if let Err(e) = feedback::record(
                    &db,
                    commit.execution_id,
                    &result.action,
                    result.success,
                    &feedback::action_target(&action),
                    &baseline,
                )
                .await
                {
                    error!("Failed to record feedback: {e}");
                }
            }

            // Always report back — even on timeout/panic — so the loop clears the
            // in_flight label and never latches the tray on "Executing". If the loop
            // has gone away the whole service is shutting down, so ignore the error.
            let _ = done_tx.send(ExecOutcome {
                key,
                label,
                exec_action: result.action,
                success: result.success,
                output: result.output,
                diagnosis,
                confidence,
                reason,
                undo_id,
                cleared_undo_id: None,
                refresh_pending,
            });
        }
    })
}

/// Route a user-initiated fix-action (a disk cleanup or startup toggle) through the
/// SAME policy gate as an AI-proposed fix: evaluate at full confidence, then auto-run a
/// whitelisted/reversible action via the executor worker or queue anything disruptive
/// for approval. The UI only ever sends an opaque id; the action is reconstructed
/// server-side, so this never widens the pipe's trust model. Does NOT broadcast — the
/// caller broadcasts once after.
///
/// `force_approval` downgrades a whitelist AutoApprove to RequireApproval — used for
/// startup toggles so that a task enable/disable (whitelisted for the AI path) gets the
/// same human confirmation as a `StartupSet`.
#[allow(clippy::too_many_arguments)]
async fn route_user_action(
    st: &mut SvcState,
    pol: &policy::ExecutionPolicy,
    db: &SqlitePool,
    exec_tx: &tokio::sync::mpsc::UnboundedSender<ExecJob>,
    action: FixAction,
    diagnosis: &str,
    reason_label: &str,
    force_approval: bool,
) -> Result<String, String> {
    let label = format!("{action:?}");
    // Dedup on the semantic key, not the label: parameters can differ while the
    // targeted issue is the same (see FixAction::dedup_key).
    let dedup = action.dedup_key();
    if st.in_flight.contains(&dedup) || st.pending.iter().any(|p| p.action.dedup_key() == dedup) {
        info!(action = %label, "User action already queued or executing — skipping duplicate");
        return Err("Action is already queued or executing".to_string());
    }
    let decision_id = match audit::log_manual_decision(db, diagnosis).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to log manual decision: {e}");
            return Err(format!("Action could not be recorded: {e}"));
        }
    };
    // A zeroed baseline: a user-requested action isn't scored for effectiveness the way
    // an autonomous fix is, so an accurate "before" snapshot isn't needed here.
    let baseline = SystemState::default();
    let verdict = match pol.evaluate(&action, 1.0) {
        policy::Verdict::AutoApprove if force_approval => {
            policy::Verdict::RequireApproval("User-initiated change — confirm to apply".to_string())
        }
        v => v,
    };
    let result = match verdict {
        policy::Verdict::Block(reason) => {
            info!(reason = %reason, action = %label, "User action blocked by policy");
            push_problem(
                st,
                diagnosis,
                1.0,
                &label,
                true,
                false,
                Some(reason.clone()),
            );
            Err(reason)
        }
        policy::Verdict::AutoApprove => {
            match safety::rate_limited(db, &action, pol.execution.rate_limit_mins).await {
                Ok(true) => {
                    // Surface a reason in the activity feed rather than silently doing
                    // nothing — a user who clicked expects feedback (the AI path can skip
                    // quietly; a manual click can't).
                    info!(action = %label, "User action rate-limited — skipping");
                    push_problem(
                        st,
                        diagnosis,
                        1.0,
                        &label,
                        true,
                        false,
                        Some("Skipped — already done recently (rate-limited).".into()),
                    );
                    return Err("Action was already completed recently".to_string());
                }
                Err(e) => {
                    warn!("Rate limit check failed — skipping user action to fail safe: {e}");
                    return Err(format!("Safety check failed: {e}"));
                }
                Ok(false) => {}
            }
            st.in_flight.insert(dedup.clone());
            if let Err(e) = exec_tx.send(ExecJob {
                action,
                decision_id,
                baseline,
                key: dedup.clone(),
                label: label.clone(),
                diagnosis: diagnosis.to_string(),
                confidence: 1.0,
                reason: Some(reason_label.to_string()),
                approval_id: None,
            }) {
                warn!("Executor worker unavailable: {e}");
                st.in_flight.remove(&dedup);
                Err("Executor is unavailable".to_string())
            } else {
                Ok("Action queued".to_string())
            }
        }
        policy::Verdict::RequireApproval(reason) => {
            let explanation = explain::explain(&action);
            let info = ApprovalInfo {
                id: 0,
                diagnosis: diagnosis.to_string(),
                root_cause: String::new(),
                confidence: 1.0,
                action: label.clone(),
                reason,
                side_effects: String::new(),
                undo_instructions: String::new(),
                action_summary: explanation.summary,
                target: explanation.target,
                target_details: explain::target_details(&action).await,
                reversible: explanation.reversible,
                created_at: chrono::Utc::now().timestamp(),
            };
            match audit::insert_pending_approval(db, decision_id, &action, &info, &baseline).await {
                Ok(row_id) => {
                    let mut info = info;
                    info.id = row_id;
                    st.pending.push(PendingApproval {
                        info,
                        action,
                        decision_id,
                        baseline,
                    });
                    Ok("Action queued for approval".to_string())
                }
                Err(e) => {
                    error!("Failed to queue user action for approval: {e}");
                    push_problem(
                        st,
                        diagnosis,
                        1.0,
                        &label,
                        true,
                        false,
                        Some(format!("could not queue for approval: {e}")),
                    );
                    Err(format!("Action could not be queued for approval: {e}"))
                }
            }
        }
    };
    st.status = resting_status(st);
    result
}

/// Fingerprint of the *actionable* signals in a snapshot — error-level log
/// events, warning/error Windows events, failed services, and resource
/// thresholds. Returns None when nothing is worth analysing, so the decision
/// loop can skip the Claude call (benign file writes and Information events are
/// ignored). Identical fingerprints across cycles mean nothing changed.
fn actionable_fingerprint(snap: &SignalSnapshot) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for fc in &snap.file_changes {
        if let Some(le) = &fc.log_event {
            if le.is_actionable() {
                parts.push(format!(
                    "F|{}|{}|{}",
                    le.log_path,
                    le.severity,
                    le.error_snippets.len()
                ));
            }
        }
    }
    for e in &snap.event_log {
        if e.is_actionable() {
            parts.push(format!("E|{}|{}|{}", e.level, e.source, e.event_id));
        }
    }
    let sys = &snap.system_state;
    // Faults (failed services, firewall off, Defender faults) share their
    // definition with the WMI reactive trigger — see SystemState::fault_parts.
    parts.extend(sys.fault_parts());
    if sys.cpu_usage_percent > 90.0 {
        parts.push("CPU>90".into());
    }
    if sys.memory_usage_percent > 90.0 {
        parts.push("MEM>90".into());
    }
    if sys.disk_usage_percent > 90.0 {
        parts.push("DISK>90".into());
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    Some(parts.join("\n"))
}

/// Copy a WMI snapshot's live metrics (cpu/mem/disk + failed services) into the broadcast
/// state. This is the single per-tick writer of these fields, sourced from
/// `wmi::current(&wmi_shared)`. The load-bearing invariant: whatever this reads from the
/// WMI cache becomes the broadcast truth — so a manual `RefreshStatus` must update the WMI
/// cache too (not just `st.failed_services`), or the next tick reverts it. Extracted so
/// that seam is unit-testable (`manual_refresh_survives_a_following_tick`, the F1 regression).
fn apply_live_metrics(st: &mut SvcState, sys: &SystemState) {
    st.cpu = sys.cpu_usage_percent;
    st.memory = sys.memory_usage_percent;
    st.disk = sys.disk_usage_percent;
    st.failed_services = sys.failed_services.clone();
    st.signals_at = sys.collected_at;
    st.signal_errors = sys.collector_errors.clone();
}

fn push_problem(
    st: &mut SvcState,
    diagnosis: &str,
    confidence: f32,
    action: &str,
    blocked: bool,
    auto_executed: bool,
    reason: Option<String>,
) {
    if st.recent_problems.len() >= 20 {
        st.recent_problems.pop_front();
    }
    st.recent_problems.push_back(ProblemSummary {
        diagnosis: diagnosis.to_string(),
        confidence,
        action: action.to_string(),
        blocked,
        auto_executed,
        reason,
        at: chrono::Utc::now().timestamp(),
    });
}

fn push_execution(
    st: &mut SvcState,
    action: &str,
    success: bool,
    output: &str,
    undo_id: Option<i64>,
) {
    let preview = output.chars().take(120).collect::<String>();
    if st.recent_executions.len() >= 20 {
        st.recent_executions.pop_front();
    }
    st.recent_executions.push_back(ExecutionSummary {
        action: action.to_string(),
        success,
        preview,
        at: chrono::Utc::now().timestamp(),
        undo_id,
    });
}

// ── Decision loop ─────────────────────────────────────────────────────────────

async fn eir_main<F: std::future::Future<Output = ()>>(shutdown: F, portable_pipe: Option<String>) {
    // Log to a file next to the executable. A Windows service has no console, so
    // stdout is discarded — the file is the only way to see what the service did.
    let log_dir = config::resolve(".");
    let file_appender = tracing_appender::rolling::never(&log_dir, "eir.log");
    let (file_writer, log_guard) = tracing_appender::non_blocking(file_appender);
    // Keep the writer worker alive for the whole process.
    std::mem::forget(log_guard);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(file_writer)
        .with_target(false)
        .init();

    let portable_mode = portable_pipe.is_some();
    let (pipe, mut ui_rx) = match portable_pipe {
        Some(pipe_name) => pipe_server::spawn_portable(pipe_name),
        None => pipe_server::spawn(),
    };
    let mut st = SvcState::default();

    macro_rules! fatal {
        ($msg:expr) => {{
            let m = $msg;
            error!("{m}");
            st.status = "Error".to_string();
            st.error = Some(m.clone());
            pipe.broadcast_status(build_status(&st));
            return;
        }};
    }

    let mut cfg = match config::load("config.toml") {
        Ok(c) => c,
        Err(e) => fatal!(format!("config.toml: {e}")),
    };
    st.settings = Some(cfg.to_ui_settings());

    let mut pol = match policy::ExecutionPolicy::load(
        config::resolve("policy.toml")
            .to_str()
            .unwrap_or("policy.toml"),
    ) {
        Ok(p) => p,
        Err(e) => fatal!(format!("policy.toml: {e}")),
    };
    // The live confidence threshold is the user-editable config value; policy.toml
    // only provides the fallback default.
    pol.execution.confidence_threshold = cfg.monitoring.confidence_threshold;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        threshold = pol.execution.confidence_threshold,
        rate_limit_mins = pol.execution.rate_limit_mins,
        "Starting Eir — service mode"
    );

    let db_path = config::resolve(&cfg.persistence.audit_db);
    let db = match audit::init_db(db_path.to_str().unwrap_or(&cfg.persistence.audit_db)).await {
        Ok(d) => d,
        Err(e) => fatal!(format!("DB init: {e}")),
    };
    // Crash-safe Game Mode power restore: if the service died mid-game with a boosted power
    // plan, the pre-boost scheme GUID is persisted — restore it now.
    game_mode::restore(&db).await;
    // Power-plan edges must run in the same order the decision loop observes them.
    // Spawning each edge independently let a rapid on→off race restore first and then
    // apply the boost, potentially leaving High Performance active with no saved undo.
    let (game_mode_edge_tx, mut game_mode_edge_rx) =
        tokio::sync::mpsc::unbounded_channel::<(bool, bool)>();
    let game_mode_db = db.clone();
    let game_mode_edge_handle = tokio::spawn(async move {
        while let Some((power_boost, on)) = game_mode_edge_rx.recv().await {
            game_mode::on_gaming_edge(power_boost, on, &game_mode_db).await;
        }
    });
    // Seed the updater status from config + history, and clear any stale install
    // staging left by a previous run.
    updater::download::cleanup_stale_staging_async().await;
    st.updater = UpdaterStatus {
        enabled: cfg.updater.enabled,
        settings: cfg.updater.to_view(),
        app_notes: cfg.updater.note_views(),
        ..Default::default()
    };
    let persisted_last_run = updater::history::last_run(&db).await.unwrap_or(0);
    let last_clean_run = updater::history::last_clean_run(&db).await.unwrap_or(0);
    let mut recent_last_run = 0;
    if let Ok(recent) = updater::history::recent(&db, 50).await {
        recent_last_run = recent.iter().map(|r| r.at).max().unwrap_or(0);
        st.updater.recent = recent;
    }
    // The persisted full-run time is authoritative. Attempt history also contains
    // targeted retries, which must not postpone the next whole-machine cycle.
    st.updater.last_run = if persisted_last_run > 0 {
        persisted_last_run
    } else {
        recent_last_run.max(last_clean_run)
    };
    st.updater.last_clean_run = last_clean_run;
    let restored_cycle = match updater::history::last_cycle_status(&db).await {
        Ok(Some((at, notes, mut apps))) if at == st.updater.last_run => {
            for app in &mut apps {
                app.ignored = cfg
                    .updater
                    .ignored
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&app.id));
                app.note = cfg.updater.notes.get(&app.id).cloned().unwrap_or_default();
            }
            st.updater.notes = notes;
            st.updater.apps = apps;
            true
        }
        Ok(_) => false,
        Err(e) => {
            warn!("Failed to restore update-cycle evidence: {e}");
            false
        }
    };
    if !restored_cycle
        && st.updater.last_run > 0
        && st.updater.last_clean_run != st.updater.last_run
    {
        let detail = "The last update check was incomplete; run it again for fresh evidence.";
        st.updater.notes = vec![detail.to_string()];
        st.updater.apps = vec![updater::orchestrator::warning_row(detail)];
    }
    st.advisor = Some(AdvisorStatus {
        enabled: cfg.advisor.enabled,
        settings: cfg.advisor.to_view(),
        ..Default::default()
    });
    // Seed today's advisor spend/count so visibility and the count cap survive a
    // settings-triggered service restart.
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    if let Ok((spent, escalations)) = audit::load_advisor_day(&db, &today).await {
        st.advisor_spent_today = spent;
        st.advisor_escalations_today = escalations;
    }
    st.advisor_spend_date = today;
    // Load the latest weekly health digest so the UI shows it immediately and the
    // weekly gate knows when the last one ran (survives restarts).
    if let Ok(Some((text, at))) = audit::latest_digest(&db).await {
        st.last_digest_at = at;
        st.digest = Some(eir_proto::DigestView {
            text: digest::bound_digest(text),
            generated_at: at,
        });
    }
    // A bad AI config must NOT kill the service — degrade instead, so the pipe
    // and UI stay alive and the user can fix it in Settings.
    let mut ai: Option<std::sync::Arc<ai::client::AiClient>> =
        match ai::client::AiClient::new(&cfg.api).await {
            Ok(c) => Some(std::sync::Arc::new(c)),
            Err(e) => {
                error!("AI client init failed: {e}");
                st.status = "Error".to_string();
                st.error = Some(format!(
                    "AI provider not configured: {e} — fix it in Settings"
                ));
                None
            }
        };

    // Reactive-guardian trigger: collectors ping this the moment they capture
    // something actionable, so fixes start within seconds of an error landing
    // instead of on the next scheduled tick. Capacity 1 + try_send coalesces
    // bursts.
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (event_log_shared, _el_shutdown) = signals::event_log::spawn(
        cfg.monitoring.event_log_channels.clone(),
        cfg.monitoring.event_log_poll_interval_secs,
        trigger_tx.clone(),
    );
    let watch_extra_dirs = cfg.monitoring.log_directories.clone();
    let initial_extra = watch_extra_dirs.clone();
    let initial_watch =
        tokio::task::spawn_blocking(move || discover_watch_dirs_for_stable_user(&initial_extra))
            .await
            .ok()
            .flatten();
    let (initial_watch_sid, initial_watch_dirs) = initial_watch
        .map(|(sid, directories)| (Some(sid), directories))
        .unwrap_or_default();
    info!(
        count = initial_watch_dirs.len(),
        "Log directories auto-discovered"
    );
    let mut known_watch_dirs: HashSet<PathBuf> = initial_watch_dirs.iter().cloned().collect();
    let (file_watch_shared, _fw_shutdown, dir_update_tx) =
        signals::file_watch::spawn(initial_watch_dirs, trigger_tx.clone());
    spawn_watch_session_reconciler(initial_watch_sid, watch_extra_dirs, dir_update_tx.clone());
    let (wmi_shared, _wmi_shutdown) =
        signals::wmi::spawn(cfg.monitoring.wmi_poll_interval_secs, trigger_tx);

    tokio::time::sleep(Duration::from_secs(5)).await;

    // Don't clobber the AI-init failure set above: with `ai = None` every cycle
    // skips analysis before any error-clearing path, so this startup settle was
    // the only writer — clearing it here left a mis-configured provider looking
    // healthy ("Active", no error) forever.
    if ai.is_some() {
        st.status = "Active".to_string();
        st.error = None;
    }
    if let Ok(s) = audit::usage_summary(&db).await {
        st.usage = Some(s);
    }
    // An `approved` row may already have produced an external side effect before a
    // crash. Replaying it cannot be made transactionally at-most-once, so require a
    // fresh click after every restart.
    match audit::requeue_claimed_approvals_on_startup(&db).await {
        Ok(count) if count > 0 => {
            warn!(
                count,
                "Requeued interrupted approved actions for fresh user approval"
            );
        }
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to recover interrupted approvals: {e}");
            st.status = "Error".to_string();
            st.error = Some(format!("Interrupted approvals could not be recovered: {e}"));
        }
    }
    // Restore approvals queued before the last restart (e.g. a settings change)
    // so the user can still act on them — they are not lost on restart.
    match audit::load_pending_approvals(&db).await {
        Ok(pending) => {
            if !pending.is_empty() {
                info!(
                    count = pending.len(),
                    "Restored pending approvals from previous run"
                );
            }
            st.pending = pending;
        }
        Err(e) => warn!("Failed to load pending approvals: {e}"),
    }
    if ai.is_some() && st.error.is_none() {
        st.status = resting_status(&st);
    }
    pipe.broadcast_status(build_status(&st));

    let mut ticker = interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));
    info!(
        interval_secs = cfg.monitoring.decision_interval_secs,
        "Decision loop started"
    );
    // Finished update cycles report back here; an in-flight cycle never blocks the loop.
    let (update_done_tx, mut update_done_rx) =
        tokio::sync::mpsc::channel::<updater::orchestrator::CycleSummary>(2);
    // Coarse live progress ("checking…", "updating {app}…") from a running cycle, so
    // the UI's phase label tracks reality instead of freezing on the start label.
    let (update_progress_tx, mut update_progress_rx) = tokio::sync::mpsc::channel::<String>(16);
    // Fix actions run on a dedicated serialised worker off the loop; jobs go out on
    // exec_tx and finished outcomes come back on exec_done_rx (drained below), so a
    // slow repair never stalls UI commands or status folding. Unbounded sends never
    // block the loop (job volume is bounded by problems-per-cycle plus approvals).
    let (exec_tx, exec_rx) = tokio::sync::mpsc::unbounded_channel::<ExecJob>();
    let (exec_done_tx, mut exec_done_rx) = tokio::sync::mpsc::unbounded_channel::<ExecOutcome>();
    // A clone for the user-triggered registry undo, which reports its outcome back
    // through the same exec_done arm (so it shows in the activity feed) instead of
    // blocking ui_rx on the restore's PowerShell call.
    let undo_done_tx = exec_done_tx.clone();
    let exec_handle = spawn_executor(&db, exec_rx, exec_done_tx);
    // AI analysis (and its optional advisor escalation) runs off the loop too — see
    // the analysis_done_rx arm below — so a multi-minute call never delays ui_rx.
    let (analysis_done_tx, mut analysis_done_rx) =
        tokio::sync::mpsc::channel::<Result<AnalysisSuccess, String>>(2);
    // The weekly health digest is one bounded AI call; it runs off-loop and reports the
    // rendered digest (plus its usage) back through this channel.
    let (digest_done_tx, mut digest_done_rx) =
        tokio::sync::mpsc::unbounded_channel::<(eir_proto::DigestView, Option<CallUsage>)>();
    // "Ask Eir" answers one free-text question off the loop; the answer (or an error
    // string) comes back here paired with the original question.
    #[allow(clippy::type_complexity)]
    let (ask_done_tx, mut ask_done_rx) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        Vec<String>, // attachment labels, for the history entry
        Result<(String, Option<CallUsage>), String>,
    )>();
    // On-demand disk scan runs off the loop (blocking walkdir) and reports its ranked
    // result back here.
    let (disk_done_tx, mut disk_done_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<disk_scan::DiskScanResult, String>>();
    // On-demand startup scan (registry/folder enumeration + optional AI classify) runs
    // off the loop and reports back here.
    let (startup_done_tx, mut startup_done_rx) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        Result<startup_scan::StartupScanResult, String>,
    )>();
    /// Regenerate the digest weekly.
    const DIGEST_INTERVAL_SECS: i64 = 7 * 24 * 3600;
    let mut analysis_running = false;
    // Watchdog on a single analysis task. Must exceed the worst-case *legitimate*
    // duration or it aborts a still-progressing call and discards its (already-billed)
    // result. A base analyze() can retry twice — up to ~3×300 s HTTP timeout + backoff
    // ≈ 15 min — and a following advisor escalation is another full call, so the honest
    // worst case is ~30 min. This is a hang backstop, not a normal-operation limit;
    // analysis_running only blocks a second overlapping analysis, never ui_rx.
    const ANALYSIS_MAX: Duration = Duration::from_secs(30 * 60);
    // The labeller is a small text-only call; bound it well under the analysis cap so
    // a wedged CLI subprocess can't hold the labelling flag for long.
    const LABEL_MAX: Duration = Duration::from_secs(3 * 60);
    // The Tier-2 learned-fact labeller is also an AI call, so it runs off the loop
    // (fire-and-forget). This flag stops it stacking; the stored explanation is
    // surfaced by the next facts_for_view refresh.
    let labelling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider_test_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cycle_count = 0u64;
    // Reactive-trigger pacing: a trigger *schedules* a reaction (react_at)
    // rather than running one inline — the debounce lets an error burst
    // coalesce into one analysis, the min-gap keeps reactions at most once a
    // minute, and because it's a deadline (not a sleep inside the select arm)
    // UI commands and executor outcomes stay responsive throughout, and a
    // trigger landing inside the gap is deferred, never dropped.
    const REACTIVE_DEBOUNCE: Duration = Duration::from_secs(10);
    const REACTIVE_MIN_GAP: Duration = Duration::from_secs(60);
    // Game Mode lease: the tray re-asserts SetGaming(true) on a ~30s heartbeat, so 90s
    // covers two missed beats before the lease expires and Eir resumes full guardian duty.
    const GAMING_LEASE_SECS: i64 = 90;
    let mut last_cycle_at = tokio::time::Instant::now();
    let mut react_at: Option<tokio::time::Instant> = None;
    // Last-known Game Mode state, tracked across the SetGaming handler and the per-tick
    // reconcile so a lease that lapses with no explicit off (e.g. a crashed tray) still
    // triggers the power-plan restore + a re-analysis.
    let mut was_gaming = false;
    // Last analysed actionable-signal fingerprint; identical states are skipped.
    let mut last_fingerprint: Option<String> = None;
    // When we last ran an analysis. None = never (forces a baseline run). Even on
    // a healthy/idle system we re-analyse on this heartbeat so the UI shows a
    // current "system healthy" result and the user can see it's alive.
    let mut last_analysis_at: Option<std::time::Instant> = None;
    const ANALYSIS_HEARTBEAT: Duration = Duration::from_secs(6 * 3600);

    let shutdown = std::pin::pin!(shutdown);
    tokio::select! {
        _ = async {
            loop {
                // Wait for either the next decision tick or a UI command. Commands
                // are handled as they arrive — not once per decision interval — so
                // Pause and settings changes respond promptly.
                // (Evaluated even when react_at is None; the branch below is
                // disabled by its precondition in that case.)
                let react_deadline = react_at
                    .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
                tokio::select! {
                    _ = ticker.tick() => {}
                    Some(()) = trigger_rx.recv() => {
                        // A collector saw something actionable — schedule a
                        // reaction after the debounce, but no sooner than the
                        // min-gap since the last cycle. See the pacing note at
                        // the react_at declaration.
                        if react_at.is_none() {
                            let due = tokio::time::Instant::now() + REACTIVE_DEBOUNCE;
                            react_at = Some(due.max(last_cycle_at + REACTIVE_MIN_GAP));
                        }
                        continue;
                    }
                    _ = tokio::time::sleep_until(react_deadline), if react_at.is_some() && !analysis_running => {
                        // Coalesce any pings that arrived while waiting. The decision body
                        // below clears react_at. Gated on !analysis_running so a past
                        // deadline can't busy-loop while an analysis holds the pass open —
                        // the reaction fires once the analysis completes.
                        while trigger_rx.try_recv().is_ok() {}
                        info!("Reacting to fresh actionable signals");
                        // fall through to the decision body below
                    }
                    Some(mut summary) = update_done_rx.recv() => {
                        // An update cycle finished — fold its result into the status.
                        st.updater_running = false;
                        st.updater.running = false;
                        if let Some(learning) = summary.learned_guidance.take() {
                            let mut next = cfg.clone();
                            match next
                                .updater
                                .set_learned_guidance(&learning.id, &learning.note)
                            {
                                Ok(_) => {
                                    if let Err(e) = config::save_async(&next, "config.toml").await {
                                        warn!("Failed to save learned update guidance: {e}");
                                        summary.notes.push(format!(
                                            "The update succeeded, but its guidance could not be learned: {e}"
                                        ));
                                    } else {
                                        cfg = next;
                                        st.updater.app_notes = cfg.updater.note_views();
                                    }
                                }
                                Err(e) => summary.notes.push(format!(
                                    "The update succeeded, but its guidance was invalid: {e}"
                                )),
                            }
                        }
                        let mut apps =
                            updater::orchestrator::app_rows(&summary, &cfg.updater);
                        if summary.targeted {
                            for app in apps.drain(..) {
                                if let Some(existing) = st
                                    .updater
                                    .apps
                                    .iter_mut()
                                    .find(|existing| existing.id.eq_ignore_ascii_case(&app.id))
                                {
                                    *existing = app;
                                } else {
                                    st.updater.apps.push(app);
                                }
                            }
                            st.updater.notes = summary.notes.clone();
                        } else {
                            st.updater.last_run = summary.completed_at;
                            if summary.clean_at > 0 {
                                st.updater.last_clean_run = summary.clean_at;
                            }
                            st.updater.notes = summary.notes.clone();
                            st.updater.apps = apps;
                        }
                        let status_at = if summary.targeted {
                            st.updater.last_run
                        } else {
                            summary.completed_at
                        };
                        if status_at > 0 {
                            if let Err(e) = updater::history::record_cycle_status(
                                &db,
                                status_at,
                                &st.updater.notes,
                                &st.updater.apps,
                            )
                            .await
                            {
                                warn!("Failed to persist update completion: {e}");
                                st.updater
                                    .notes
                                    .push(format!("failed to persist update completion: {e}"));
                            }
                        }
                        st.updater.last_cost_usd = summary.cost_usd;
                        st.updater.phase = "idle".to_string();
                        if let Ok(recent) = updater::history::recent(&db, 50).await {
                            st.updater.recent = recent;
                        }
                        st.updater.next_run = if cfg.updater.enabled {
                            st.updater
                                .last_run
                                .saturating_add(cfg.updater.schedule_interval_secs as i64)
                        } else {
                            0
                        };
                        info!(
                            apps = st.updater.apps.len(),
                            targeted = summary.targeted,
                            "Update cycle complete"
                        );
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some((view, usage)) = digest_done_rx.recv() => {
                        // A weekly digest task finished. Empty text == failure: clear the
                        // flag and push the next attempt out a week, but keep any prior
                        // digest and don't bill. Otherwise persist, bill, and show it.
                        st.digest_running = false;
                        st.last_digest_at = view.generated_at;
                        if !view.text.is_empty() {
                            if let Some(u) = usage {
                                if let Err(e) = audit::log_usage(&db, &u).await {
                                    warn!("Failed to log digest usage: {e}");
                                }
                            }
                            info!("Weekly health digest generated");
                            st.digest = Some(view);
                            pipe.broadcast_status(build_status(&st));
                        }
                        continue;
                    }
                    Some((scan_sid, result)) = startup_done_rx.recv() => {
                        // A switched user's result must never replace or reveal the
                        // current user's cache. A newer scan may already own the slot.
                        let active_sid = executor::startup::active_user_sid().ok();
                        if !startup_owner_matches(
                            st.startup_owner_sid.as_deref(),
                            Some(&scan_sid),
                        ) || !startup_owner_matches(Some(&scan_sid), active_sid.as_deref())
                        {
                            if startup_owner_matches(
                                st.startup_owner_sid.as_deref(),
                                Some(&scan_sid),
                            ) {
                                clear_stale_startup_cache(&mut st, active_sid.as_deref());
                            }
                            warn!("Discarded startup scan result for a non-active user");
                            pipe.broadcast_status(build_status(&st));
                            continue;
                        }
                        if let Ok(res) = &result {
                            if !res.owner_sid.eq_ignore_ascii_case(&scan_sid) {
                                clear_stale_startup_cache(&mut st, None);
                                warn!("Discarded startup scan result with mismatched owner SID");
                                pipe.broadcast_status(build_status(&st));
                                continue;
                            }
                        }

                        // This active user's startup scan finished. Fold in the entries
                        // and opaque id→toggle map.
                        st.startup_scan_running = false;
                        let now = chrono::Utc::now().timestamp();
                        match result {
                            Ok(res) => {
                                if let Some(u) = res.usage {
                                    if let Err(e) = audit::log_usage(&db, &u).await {
                                        warn!("Failed to log startup-classify usage: {e}");
                                    }
                                    match audit::usage_summary(&db).await {
                                        Ok(s) => st.usage = Some(s),
                                        Err(e) => warn!("Failed to compute usage summary: {e}"),
                                    }
                                }
                                st.startup_targets = res.targets;
                                st.startup = Some(eir_proto::StartupView {
                                    running: false,
                                    scanned_at: now,
                                    error: None,
                                    entries: res.entries,
                                });
                            }
                            Err(e) => {
                                warn!("Startup scan failed: {e}");
                                let prev = st.startup.take();
                                st.startup = Some(eir_proto::StartupView {
                                    running: false,
                                    scanned_at: prev.as_ref().map(|s| s.scanned_at).unwrap_or(0),
                                    error: Some(e),
                                    entries: prev.map(|s| s.entries).unwrap_or_default(),
                                });
                            }
                        }
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some(result) = disk_done_rx.recv() => {
                        // A disk scan finished. Fold in the ranked entries + the id→action
                        // map (used to reconstruct a "Clean" click server-side).
                        st.disk_scan_running = false;
                        let now = chrono::Utc::now().timestamp();
                        match result {
                            Ok(res) => {
                                st.disk_targets = res.targets;
                                st.disk_insights = Some(eir_proto::DiskInsightsView {
                                    running: false,
                                    scanned_at: now,
                                    error: None,
                                    entries: res.entries,
                                });
                            }
                            Err(e) => {
                                warn!("Disk scan failed: {e}");
                                let prev = st.disk_insights.take();
                                st.disk_insights = Some(eir_proto::DiskInsightsView {
                                    running: false,
                                    scanned_at: prev.as_ref().map(|d| d.scanned_at).unwrap_or(0),
                                    error: Some(e),
                                    entries: prev.map(|d| d.entries).unwrap_or_default(),
                                });
                            }
                        }
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some((question, attachments, result)) = ask_done_rx.recv() => {
                        // An "Ask Eir" answer finished (or failed). Fold it into the Q&A
                        // history; never touch st.error (same isolation as exec_done_rx).
                        st.ask_running = false;
                        st.last_ask_at = chrono::Utc::now().timestamp();
                        match result {
                            Ok((answer, usage)) => {
                                if let Some(u) = usage {
                                    if let Err(e) = audit::log_usage(&db, &u).await {
                                        warn!("Failed to log ask usage: {e}");
                                    }
                                    match audit::usage_summary(&db).await {
                                        Ok(s) => st.usage = Some(s),
                                        Err(e) => warn!("Failed to compute usage summary: {e}"),
                                    }
                                }
                                if st.ask_entries.len() >= 10 {
                                    st.ask_entries.pop_back();
                                }
                                st.ask_entries.push_front(eir_proto::AskEntry {
                                    question,
                                    answer: ask::bound_answer(answer),
                                    at: chrono::Utc::now().timestamp(),
                                    attachments,
                                });
                                // Preserve a rejection notice that arrived while this
                                // answer was in flight (a duplicate submission rejected
                                // mid-run) — clearing it here would erase the only signal
                                // the user gets, since the client masks it while running.
                                // Starting an ask already cleared any prior error, so the
                                // only error present now is such a mid-flight rejection.
                                let carry = st.ask.as_ref().and_then(|a| a.error.clone());
                                refresh_ask(&mut st, carry);
                            }
                            Err(e) => {
                                warn!("Ask Eir failed: {e}");
                                refresh_ask(&mut st, Some(format!("Couldn't answer that: {e}")));
                            }
                        }
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some(phase) = update_progress_rx.recv() => {
                        // Live stage of a running cycle. Guarded on `updater_running` so a
                        // straggling message can't overwrite the "idle" a just-finished
                        // cycle set.
                        if st.updater_running {
                            st.updater.phase = phase;
                            pipe.broadcast_status(build_status(&st));
                        }
                        continue;
                    }
                    Some(outcome) = exec_done_rx.recv() => {
                        // A fix action finished on the worker — fold its result in.
                        st.in_flight.remove(&outcome.key);
                        if outcome.refresh_pending {
                            match audit::load_pending_approvals(&db).await {
                                Ok(pending) => st.pending = pending,
                                Err(e) => {
                                    warn!("Failed to reload requeued approvals: {e}");
                                }
                            }
                        }
                        // If this was a successful undo, retire the original row's Undo
                        // button so it can't be clicked again (it would no-op).
                        if let Some(cid) = outcome.cleared_undo_id {
                            for ex in st.recent_executions.iter_mut() {
                                if ex.undo_id == Some(cid) {
                                    ex.undo_id = None;
                                }
                            }
                        }
                        push_execution(
                            &mut st,
                            &outcome.exec_action,
                            outcome.success,
                            &outcome.output,
                            outcome.undo_id,
                        );
                        push_problem(
                            &mut st,
                            &outcome.diagnosis,
                            outcome.confidence,
                            &outcome.label,
                            false,
                            true,
                            outcome.reason,
                        );
                        // Don't touch st.error here: an execution outcome must not wipe
                        // an unrelated AI/connection error set by another path.
                        st.status = resting_status(&st);
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some(outcome) = analysis_done_rx.recv() => {
                        analysis_running = false;
                        let AnalysisSuccess {
                            snapshot,
                            fingerprint,
                            decision,
                            usage,
                            advisor,
                            advisor_spent_today,
                            advisor_escalations_today,
                            escalation_cost_usd,
                        } = match outcome {
                            Ok(s) => s,
                            Err(e) => {
                                error!("AI analysis failed: {e}");
                                // Don't clobber "Paused": while paused the per-cycle body
                                // early-continues before resting_status runs again, so an
                                // "Error" set here would latch on the tray until the user
                                // toggles pause. Every other status writer guards on paused.
                                st.status = if st.paused {
                                    "Paused".to_string()
                                } else {
                                    "Error".to_string()
                                };
                                st.error = Some(format!("AI: {e}"));
                                pipe.broadcast_status(build_status(&st));
                                continue;
                            }
                        };
                        let claude_decision = decision;

                        for u in &usage {
                            if let Err(e) = audit::log_usage(&db, u).await {
                                warn!("Failed to log usage: {e}");
                            }
                        }
                        if !usage.is_empty() {
                            match audit::usage_summary(&db).await {
                                Ok(s) => st.usage = Some(s),
                                Err(e) => warn!("Failed to compute usage summary: {e}"),
                            }
                        }

                        // Remember this state + time so unchanged idle cycles are skipped
                        // until the next heartbeat.
                        last_fingerprint = fingerprint;
                        last_analysis_at = Some(std::time::Instant::now());

                        st.last_analysis = claude_decision.analysis.clone();
                        st.last_analysis_unix = chrono::Utc::now().timestamp();
                        // Match the idle-skip/heartbeat gates: don't clear an error the
                        // user may have paused because of.
                        if !st.paused {
                            st.error = None;
                        }

                        // If the UTC day rolled over WHILE this analysis was in flight, the
                        // per-cycle rollover (bottom of the loop body) never ran — it's gated
                        // behind `if analysis_running`. Folding the task's totals back in would
                        // restore yesterday's spend/count onto the new day, misreporting spend and
                        // possibly blocking a legitimate new-day escalation at the count cap.
                        // Detect the straddle and attribute only this analysis's escalation.
                        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                        if st.advisor_spend_date != today {
                            st.advisor_spend_date = today;
                            st.advisor_spent_today = escalation_cost_usd;
                            st.advisor_escalations_today = u32::from(advisor.escalated);
                        } else {
                            st.advisor_spent_today = advisor_spent_today;
                            st.advisor_escalations_today = advisor_escalations_today;
                        }
                        // Merge only the escalation/spend fields the task computed. `advisor`
                        // was built from the config captured at spawn time; a SetAdvisorSettings
                        // that lands mid-analysis has already updated st.advisor.enabled/.settings
                        // live, so a wholesale replace here would silently revert the user's change
                        // in the UI until the next analysis (up to the 6h heartbeat).
                        match st.advisor.as_mut() {
                            Some(existing) => {
                                existing.escalated = advisor.escalated;
                                existing.escalation_model = advisor.escalation_model;
                                existing.reason = advisor.reason;
                                existing.spent_today_usd = advisor.spent_today_usd;
                            }
                            None => st.advisor = Some(advisor),
                        }
                        // Persist the day's escalation spend/count so visibility and the count
                        // cap survive a restart (see load at startup).
                        if let Err(e) = audit::save_advisor_day(
                            &db,
                            &st.advisor_spend_date,
                            st.advisor_spent_today,
                            st.advisor_escalations_today,
                        )
                        .await
                        {
                            warn!("Failed to persist advisor daily spend: {e}");
                        }

                        // learned_facts may have changed during the (possibly minutes-long) analysis;
                        // reload fresh rather than reusing the pre-spawn snapshot.
                        let learned_facts = learn::LearnedFacts::load(&db).await;

                        let decision_id = match audit::log_decision(&db, &snapshot, &claude_decision).await {
                            Ok(id) => id,
                            Err(e) => {
                                error!("Failed to write audit log: {e}");
                                continue;
                            }
                        };

                        if let Ok(rate) = safety::success_rate(&db).await {
                            info!(success_rate = format!("{:.1}%", rate * 100.0), "Execution stats");
                            if rate < 0.85 {
                                warn!(
                                    success_rate = format!("{:.1}%", rate * 100.0),
                                    "Success rate below 85% — consider raising confidence_threshold"
                                );
                            }
                        }

                        // ── Per-problem routing ──────────────────────────────────────
                        let problems_found = !claude_decision.problems.is_empty();

                        for problem in &claude_decision.problems {
                            info!(
                                confidence = problem.confidence,
                                diagnosis  = %problem.diagnosis,
                                "Problem identified"
                            );

                            let Some(action) = problem.parse_fix_action() else {
                                warn!(fix = %problem.proposed_fix, "Unknown action type — skipping");
                                push_problem(
                                    &mut st,
                                    &problem.diagnosis,
                                    problem.confidence,
                                    &problem.proposed_fix.to_string(),
                                    true,
                                    false,
                                    Some("unrecognised fix action".into()),
                                );
                                pipe.broadcast_status(build_status(&st));
                                continue;
                            };

                            // Self-improvement: shave a capped amount off confidence for an action
                            // the machine has learned is ineffective or that the user keeps
                            // rejecting, so the EXISTING policy gate accounts for it. Never lowers a
                            // security action (see apply::confidence_penalty).
                            let action_label = format!("{action:?}");
                            let confidence =
                                (problem.confidence - learned_facts.confidence_penalty(&action_label)).max(0.0);

                            match pol.evaluate(&action, confidence) {
                                policy::Verdict::Block(reason) => {
                                    info!(reason = %reason, "Blocked by policy");
                                    // Show the gate-evaluated (penalty-adjusted) confidence, not
                                    // the raw AI value, so the card doesn't contradict a reason
                                    // like "Confidence 72% below threshold 80%".
                                    push_problem(
                                        &mut st,
                                        &problem.diagnosis,
                                        confidence,
                                        &format!("{action:?}"),
                                        true,
                                        false,
                                        Some(reason),
                                    );
                                    pipe.broadcast_status(build_status(&st));
                                }

                                policy::Verdict::AutoApprove => {
                                    match safety::rate_limited(
                                        &db,
                                        &action,
                                        pol.execution.rate_limit_mins,
                                    )
                                    .await
                                    {
                                        Ok(true) => {
                                            info!(action = ?action, "Rate-limited — skipping");
                                            continue;
                                        }
                                        // Fail CLOSED: this check is also the failure
                                        // circuit breaker, so a DB error must skip the
                                        // fix this cycle rather than let it auto-run.
                                        Err(e) => {
                                            warn!("Rate limit check failed — skipping to fail safe: {e}");
                                            continue;
                                        }
                                        Ok(false) => {}
                                    }

                                    let label = format!("{action:?}");
                                    // Skip if already running OR already sitting in the
                                    // approval queue. Confidence varies per cycle (AI output
                                    // minus the learned penalty), so the same action can be
                                    // RequireApproval one cycle and AutoApprove the next;
                                    // without the pending check it would auto-run and bypass
                                    // the queued card, then run a SECOND time when the user
                                    // later approves the stale card.
                                    // Pending is matched on the semantic key, not the
                                    // label — AI-regenerated parameters must not defeat
                                    // the check (see FixAction::dedup_key).
                                    let dedup = action.dedup_key();
                                    if st.in_flight.contains(&dedup)
                                        || st.pending.iter().any(|p| p.action.dedup_key() == dedup)
                                    {
                                        info!(action = %label, "Already executing or awaiting approval — skipping duplicate");
                                        continue;
                                    }
                                    info!(action = ?action, "AUTO-EXECUTING (queued)");
                                    // Hand off to the executor worker and move on; the outcome
                                    // is folded in when it finishes (see exec_done_rx arm).
                                    st.in_flight.insert(dedup.clone());
                                    if let Err(e) = exec_tx.send(ExecJob {
                                        action: action.clone(),
                                        decision_id,
                                        baseline: snapshot.system_state.clone(),
                                        key: dedup.clone(),
                                        label: label.clone(),
                                        diagnosis: problem.diagnosis.clone(),
                                        confidence: problem.confidence,
                                        reason: None,
                                        approval_id: None,
                                    }) {
                                        // The worker is gone; no ExecOutcome will arrive, so
                                        // drop the label instead of leaking it in in_flight
                                        // (which would skip this action forever and pin the
                                        // UI to "Executing").
                                        warn!("Executor worker unavailable: {e}");
                                        st.in_flight.remove(&dedup);
                                        st.error = Some(
                                            "Executor is unavailable; the fix was not started"
                                                .to_string(),
                                        );
                                    } else {
                                        st.status = "Executing".to_string();
                                    }
                                    pipe.broadcast_status(build_status(&st));
                                }

                                policy::Verdict::RequireApproval(reason) => {
                                    // Non-blocking: queue the action (persisted to the DB)
                                    // and move on. The user can approve or reject it from
                                    // the UI whenever they like — the loop never stalls and
                                    // the approval never expires out from under them.
                                    let action_label = format!("{action:?}");
                                    // Skip if it's already queued for approval OR already running
                                    // off-loop — otherwise a re-surfacing problem could queue a
                                    // second card for an in-flight approved action and double-run it.
                                    // Pending is matched on the semantic key, not the label: the AI
                                    // re-proposes a persistent fault every analysis run with slightly
                                    // different parameters (script text, value payload), and an exact
                                    // label match would let those pile up as duplicate cards.
                                    let dedup = action.dedup_key();
                                    if st.pending.iter().any(|p| p.action.dedup_key() == dedup)
                                        || st.in_flight.contains(&dedup)
                                    {
                                        info!(action = %action_label, "Already awaiting approval or executing — skipping duplicate");
                                        continue;
                                    }
                                    info!(reason = %reason, action = %action_label, "Queued for approval");

                                    let explanation = explain::explain(&action);
                                    let info = ApprovalInfo {
                                        id: 0, // replaced with the DB row id below
                                        diagnosis:         problem.diagnosis.clone(),
                                        root_cause:        problem.root_cause.clone(),
                                        confidence:        problem.confidence,
                                        action:            action_label.clone(),
                                        reason:            reason.clone(),
                                        side_effects:      problem.side_effects.clone(),
                                        undo_instructions: problem.undo_instructions.clone(),
                                        action_summary:    explanation.summary,
                                        target:            explanation.target,
                                        target_details:    explain::target_details(&action).await,
                                        reversible:        explanation.reversible,
                                        created_at:        chrono::Utc::now().timestamp(),
                                    };

                                    match audit::insert_pending_approval(
                                        &db,
                                        decision_id,
                                        &action,
                                        &info,
                                        &snapshot.system_state,
                                    )
                                    .await
                                    {
                                        Ok(row_id) => {
                                            let mut info = info;
                                            info.id = row_id;
                                            st.pending.push(PendingApproval {
                                                info,
                                                action: action.clone(),
                                                decision_id,
                                                baseline: snapshot.system_state.clone(),
                                            });
                                            st.status = "PendingApproval".to_string();
                                            pipe.broadcast_status(build_status(&st));
                                        }
                                        Err(e) => {
                                            // Don't lose the finding — surface it as a problem.
                                            error!("Failed to queue approval: {e}");
                                            push_problem(
                                                &mut st,
                                                &problem.diagnosis,
                                                problem.confidence,
                                                &action_label,
                                                true,
                                                false,
                                                Some(format!("could not queue for approval: {e}")),
                                            );
                                            pipe.broadcast_status(build_status(&st));
                                        }
                                    }
                                }
                            }
                        }

                        let tray_status = if st.paused {
                            "Paused"
                        } else if !st.pending.is_empty() {
                            "PendingApproval"
                        } else if !st.in_flight.is_empty() {
                            "Executing"
                        } else if problems_found {
                            "Warning"
                        } else {
                            "Active"
                        }
                        .to_string();
                        if !st.paused {
                            st.status = tray_status;
                        }
                        pipe.broadcast_status(build_status(&st));
                        continue;
                    }
                    Some(request) = ui_rx.recv() => {
                        let mut request_id = request.request_id;
                        let mut command_result = Ok("Command applied".to_string());
                        match request.command {
                        UiMsg::TogglePause => {
                            st.paused = !st.paused;
                            st.status = resting_status(&st);
                            command_result = Ok(if st.paused {
                                "Monitoring paused".to_string()
                            } else {
                                "Monitoring resumed".to_string()
                            });
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::ClearProblems => {
                            st.recent_problems.clear();
                            command_result = Ok("Problems cleared".to_string());
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::ClearExecutions => {
                            st.recent_executions.clear();
                            command_result = Ok("Activity cleared".to_string());
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::RefreshStatus => {
                            // Force a fresh services scan so a recovered service clears
                            // immediately instead of waiting for the next WMI poll +
                            // decision tick, then re-settle the hero/tray. Services-only
                            // (sub-second) so awaiting it inline doesn't stall the loop;
                            // cpu/mem/disk refresh on the normal cadence. Deliberately does
                            // NOT clear st.error — that banner is a live AI/config error
                            // with its own lifecycle, not stale status.
                            st.failed_services =
                                signals::wmi::rescan_failed_services(&wmi_shared).await;
                            // Re-settle the transient "Warning" to a resting status, but
                            // never downgrade a live "Error" (resting_status has no Error
                            // tier, so calling it while st.error is set would show "Active"
                            // over an unresolved error banner).
                            if st.status != "Error" {
                                st.status = resting_status(&st);
                            }
                            command_result = Ok("Status refreshed".to_string());
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::SetGaming { on, manual } => {
                            let now = chrono::Utc::now().timestamp();
                            let was = is_gaming_at(&st, now);
                            apply_set_gaming(&mut st, on, manual, now, GAMING_LEASE_SECS);
                            // Drive the power plan off the *overall* gaming transition, not
                            // the raw `on` flag — an auto off (exit debounce) while the
                            // manual latch is still set must NOT restore power mid-game.
                            // (A manual off clears both inputs, so it does restore.)
                            let is_now = is_gaming_at(&st, now);
                            // Queue power-plan edges off-loop — powercfg can take up to 10s,
                            // and the single worker preserves rapid on/off ordering.
                            if !was && is_now {
                                // Gaming started: apply the power boost (Phase 2, only if the
                                // opt-in setting is on). Applied once per session.
                                was_gaming = true;
                                let power_boost = cfg.monitoring.game_mode_power_boost;
                                let _ = game_mode_edge_tx.send((power_boost, true));
                            } else if was && !is_now {
                                // Gaming ended: restore the power plan (a no-op if not
                                // boosted), and force a fresh analysis (an unchanged
                                // fingerprint would otherwise be idle-skipped) + a prompt
                                // reaction, so anything noticed during the game is handled now.
                                let _ = game_mode_edge_tx.send((false, false));
                                was_gaming = false;
                                last_fingerprint = None;
                                if react_at.is_none() {
                                    react_at =
                                        Some(tokio::time::Instant::now() + REACTIVE_DEBOUNCE);
                                }
                            }
                            st.status = resting_status(&st);
                            command_result = Ok(if is_now {
                                "Game Mode enabled".to_string()
                            } else {
                                "Game Mode disabled".to_string()
                            });
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::TestProvider => {
                            if provider_test_running
                                .compare_exchange(
                                    false,
                                    true,
                                    std::sync::atomic::Ordering::AcqRel,
                                    std::sync::atomic::Ordering::Acquire,
                                )
                                .is_err()
                            {
                                command_result =
                                    Err("A provider test is already running".to_string());
                            } else {
                            match ai::client::AiClient::new(&cfg.api).await {
                                Err(e) => {
                                    provider_test_running
                                        .store(false, std::sync::atomic::Ordering::Release);
                                    command_result =
                                        Err(format!("Provider configuration is invalid: {e}"));
                                }
                                Ok(client) => {
                                    let provider = cfg.api.provider.as_str().to_string();
                                    let model = if cfg.api.model.trim().is_empty() {
                                        "provider default".to_string()
                                    } else {
                                        cfg.api.model.clone()
                                    };
                                    let pipe_test = pipe.clone();
                                    let running = provider_test_running.clone();
                                    let test_request_id = request_id.take();
                                    tokio::spawn(async move {
                                        let tested = tokio::time::timeout(
                                            Duration::from_secs(120),
                                            client.complete_text(
                                                "Reply with exactly EIR_OK.",
                                                "",
                                            ),
                                        )
                                        .await;
                                        let result = match tested {
                                            Ok(Ok((text, _))) if !text.trim().is_empty() => Ok(
                                                format!("{provider} / {model} responded successfully"),
                                            ),
                                            Ok(Ok(_)) => {
                                                Err(format!("{provider} / {model} returned no text"))
                                            }
                                            Ok(Err(e)) => {
                                                Err(format!("{provider} / {model} failed: {e}"))
                                            }
                                            Err(_) => Err(format!(
                                                "{provider} / {model} timed out after 120 seconds"
                                            )),
                                        };
                                        running.store(
                                            false,
                                            std::sync::atomic::Ordering::Release,
                                        );
                                        pipe_test.command_result(test_request_id, result);
                                    });
                                }
                            }
                            }
                        }
                        UiMsg::UpdateSettings(update) => {
                            let needs_restart = cfg.settings_update_needs_restart(&update);
                            let mut next = cfg.clone();
                            let next_ai = match next.apply_update(*update) {
                                Ok(()) => ai::client::AiClient::new(&next.api).await,
                                Err(e) => Err(e),
                            };
                            match next_ai {
                                Err(e) => {
                                    warn!("Rejected settings: {e}");
                                    st.error = Some(format!("Settings not applied — {e}"));
                                    command_result = Err(format!("Settings not applied: {e}"));
                                    pipe.broadcast_status(build_status(&st));
                                }
                                Ok(new_ai) => {
                                    if let Err(e) = config::save_async(&next, "config.toml").await {
                                        error!("Failed to save settings: {e}");
                                        st.error = Some(format!("Save settings: {e}"));
                                        command_result =
                                            Err(format!("Settings could not be saved: {e}"));
                                        pipe.broadcast_status(build_status(&st));
                                    } else {
                                        cfg = next;
                                        st.settings = Some(cfg.to_ui_settings());
                                        ai = Some(std::sync::Arc::new(new_ai));
                                        pol.execution.confidence_threshold =
                                            cfg.monitoring.confidence_threshold;
                                        ticker =
                                            interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));
                                        ticker.reset();
                                        // A stale AI-provider error is now resolved.
                                        if st
                                            .error
                                            .as_ref()
                                            .map(|s| s.starts_with("AI provider not configured"))
                                            .unwrap_or(false)
                                        {
                                            st.error = None;
                                        }
                                        st.status = resting_status(&st);
                                        command_result = Ok(match (needs_restart, portable_mode) {
                                            (true, true) => {
                                                "Settings saved; restart portable Eir to apply collector changes"
                                                    .to_string()
                                            }
                                            (true, false) => {
                                                "Settings saved; restarting service".to_string()
                                            }
                                            (false, _) => {
                                                "Settings saved and applied".to_string()
                                            }
                                        });
                                        pipe.broadcast_status(build_status(&st));
                                        if should_restart_service_after_settings(
                                            needs_restart,
                                            portable_mode,
                                        ) {
                                            info!("Settings saved — restarting service to apply collector changes");
                                            st.status = "Restarting".to_string();
                                            pipe.broadcast_status(build_status(&st));
                                            if restart_self() {
                                                if !pipe
                                                    .command_result_flushed(
                                                        request_id.take(),
                                                        command_result.clone(),
                                                    )
                                                    .await
                                                {
                                                    warn!("Settings were saved, but the UI disconnected before the restart result was delivered");
                                                }
                                                return;
                                            }
                                            st.status = resting_status(&st);
                                            st.error = Some(
                                                "Settings saved, but the restart helper failed to launch — restart EirSvc manually.".to_string(),
                                            );
                                            command_result = Err(
                                                "Settings saved, but EirSvc could not restart automatically".to_string(),
                                            );
                                            pipe.broadcast_status(build_status(&st));
                                        }
                                    }
                                }
                            }
                        }
                        UiMsg::Approve { id, approved } => {
                            // Claim first: the durable row owns the decision until a
                            // completed execution is logged, so a crash cannot lose it.
                            match audit::claim_pending_approval(&db, id, approved).await {
                            Ok(Some(pa)) => {
                                st.pending.retain(|p| p.info.id != id);
                                if approved {
                                    let label = pa.info.action.clone();
                                    let key = pa.action.dedup_key();
                                    match approved_action_ready(&pol, &db, &pa).await {
                                    Ok(()) if st.in_flight.insert(key.clone()) => {
                                        info!(action = ?pa.action, "UI-approved — queueing for execution");
                                        if let Err(e) = exec_tx.send(ExecJob {
                                            action: pa.action,
                                            decision_id: pa.decision_id,
                                            baseline: pa.baseline,
                                            key: key.clone(),
                                            label: label.clone(),
                                            diagnosis: pa.info.diagnosis.clone(),
                                            confidence: pa.info.confidence,
                                            reason: Some("approved by user".into()),
                                            approval_id: Some(id),
                                        }) {
                                            warn!("Executor worker unavailable: {e}");
                                            st.in_flight.remove(&key);
                                            command_result =
                                                Err("Executor is unavailable; approval was saved for retry".to_string());
                                        } else {
                                            command_result =
                                                Ok("Approval saved; action queued".to_string());
                                        }
                                    }
                                    Ok(()) => {
                                        info!(action = %label, "Approved action already executing — not re-running");
                                        let _ = audit::delete_pending_approval(&db, id).await;
                                        command_result =
                                            Ok("Action is already executing".to_string());
                                    }
                                    Err(ApprovalPreflightError::Resolved(reason)) => {
                                        warn!(id, %reason, "Approved action no longer passes safety preflight");
                                        let _ = audit::delete_pending_approval(&db, id).await;
                                        push_problem(
                                            &mut st,
                                            &pa.info.diagnosis,
                                            pa.info.confidence,
                                            &label,
                                            true,
                                            false,
                                            Some(reason),
                                        );
                                        command_result =
                                            Err("Action is no longer allowed by current safety policy".to_string());
                                    }
                                    Err(ApprovalPreflightError::Retry(reason)) => {
                                        warn!(id, %reason, "Approved action safety preflight could not complete");
                                        st.error = Some(reason);
                                        command_result = Err(
                                            "Approval was saved, but safety checks could not complete; it will retry after restart".to_string(),
                                        );
                                    }
                                    }
                                } else {
                                    info!(id, diagnosis = %pa.info.diagnosis, "UI-rejected");
                                    // Record the rejection so self-improvement can learn to
                                    // stop proposing an action the user keeps refusing.
                                    if let Err(e) =
                                        learn::record_rejection(&db, pa.decision_id, &pa.info.action).await
                                    {
                                        warn!("Failed to record rejection: {e}");
                                    }
                                    push_problem(
                                        &mut st,
                                        &pa.info.diagnosis,
                                        pa.info.confidence,
                                        &pa.info.action,
                                        false,
                                        false,
                                        Some("rejected by user".into()),
                                    );
                                    if let Err(e) = audit::delete_pending_approval(&db, id).await {
                                        warn!("Failed to retire rejected approval {id}: {e}");
                                    }
                                    command_result = Ok("Action rejected".to_string());
                                }
                                // Match every other error-clear site: don't wipe a live
                                // AI/config error the user may have paused because of. An
                                // Approve/Reject is unrelated to that error.
                                if !st.paused && command_result.is_ok() {
                                    st.error = None;
                                }
                            }
                            Ok(None) => {
                                info!(id, "Approval was stale or already claimed");
                                command_result =
                                    Err("Approval is stale or already resolved".to_string());
                            }
                            Err(e) => {
                                warn!("Failed to claim approval {id}: {e}");
                                st.error = Some(format!("Could not save approval decision: {e}"));
                                command_result =
                                    Err(format!("Approval decision could not be saved: {e}"));
                            }
                            }
                            // Whether resolved or stale, settle the status and
                            // refresh the UI (the card disappears from the queue).
                            st.status = resting_status(&st);
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::RunUpdatesNow => {
                            if st.paused {
                                command_result =
                                    Err("Resume monitoring before running updates".to_string());
                            } else if st.updater_running {
                                command_result =
                                    Err("An update check is already running".to_string());
                            } else {
                                st.updater_running = true;
                                st.updater.running = true;
                                st.updater.phase = "checking…".to_string();
                                command_result = Ok("Update check started".to_string());
                                pipe.broadcast_status(build_status(&st));
                                spawn_update_cycle(
                                    &cfg,
                                    &db,
                                    &update_done_tx,
                                    &update_progress_tx,
                                    None,
                                );
                            }
                        }
                        UiMsg::RetryAppUpdate { id } => {
                            let key = id.trim().to_lowercase();
                            let failed = failed_update_for_retry(&st.updater.apps, &key);
                            if st.paused {
                                command_result =
                                    Err("Resume monitoring before retrying updates".to_string());
                            } else if st.updater_running {
                                command_result =
                                    Err("An update check is already running".to_string());
                            } else if failed.is_none() {
                                command_result =
                                    Err("Only a currently failed app can be retried".to_string());
                            } else if let Some(failed) = failed {
                                let guided = cfg.updater.guidance_for(&failed.id).is_some();
                                st.updater_running = true;
                                st.updater.running = true;
                                st.updater.phase = format!("re-checking {}…", failed.name);
                                command_result = Ok(if guided {
                                    format!("Re-checking {} with saved guidance", failed.name)
                                } else {
                                    format!("Re-checking {}", failed.name)
                                });
                                pipe.broadcast_status(build_status(&st));
                                spawn_update_cycle(
                                    &cfg,
                                    &db,
                                    &update_done_tx,
                                    &update_progress_tx,
                                    Some(failed),
                                );
                            }
                        }
                        UiMsg::ClearUpdateHistory => {
                            if st.updater_running {
                                command_result =
                                    Err("Wait for the current update check to finish".to_string());
                            } else {
                                match updater::history::clear(&db).await {
                                    Err(e) => {
                                        warn!("Failed to clear update history: {e}");
                                        command_result =
                                            Err(format!("Update history could not be cleared: {e}"));
                                    }
                                    Ok(_) => {
                                        st.updater.apps.clear();
                                        st.updater.notes.clear();
                                        st.updater.recent.clear();
                                        st.updater.last_cost_usd = 0.0;
                                        if let Err(e) = learn::clear_detector_facts(&db).await {
                                            warn!("Failed to clear learned facts: {e}");
                                            command_result = Err(format!(
                                                "History cleared, but learned update facts remain: {e}"
                                            ));
                                        } else {
                                            command_result =
                                                Ok("Update history cleared".to_string());
                                        }
                                        st.learned_facts =
                                            learn::facts_for_view(&db).await.unwrap_or_default();
                                        pipe.broadcast_status(build_status(&st));
                                    }
                                }
                            }
                        }
                        UiMsg::SetLearnedFact { id, op } => {
                            if let Err(e) = learn::set_learned_fact(&db, id, &op).await {
                                warn!("Failed to set learned fact {id} ({op}): {e}");
                                command_result =
                                    Err(format!("Learned fact could not be changed: {e}"));
                            } else {
                                command_result = Ok("Learned fact updated".to_string());
                                st.learned_facts =
                                    learn::facts_for_view(&db).await.unwrap_or_default();
                                pipe.broadcast_status(build_status(&st));
                            }
                        }
                        UiMsg::UpdateUpdaterSettings(update) => {
                            let mut next = cfg.clone();
                            next.updater.apply_view(*update);
                            if let Err(e) = config::save_async(&next, "config.toml").await {
                                warn!("Failed to save updater settings: {e}");
                                command_result =
                                    Err(format!("Updater settings could not be saved: {e}"));
                            } else {
                                cfg = next;
                                st.updater.enabled = cfg.updater.enabled;
                                st.updater.settings = cfg.updater.to_view();
                                st.updater.next_run = if !cfg.updater.enabled {
                                    0
                                } else if st.updater.last_run > 0 {
                                    st.updater
                                        .last_run
                                        .saturating_add(cfg.updater.schedule_interval_secs as i64)
                                } else {
                                    chrono::Utc::now()
                                        .timestamp()
                                        .saturating_add(cfg.updater.schedule_interval_secs as i64)
                                };
                                command_result = Ok("Updater settings saved".to_string());
                                pipe.broadcast_status(build_status(&st));
                            }
                        }
                        UiMsg::SetAppIgnore { id, ignore, note } => {
                            let key = id.trim().to_lowercase();
                            let known = st
                                .updater
                                .apps
                                .iter()
                                .any(|a| a.id.eq_ignore_ascii_case(&key))
                                || cfg
                                    .updater
                                    .ignored
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(&key));
                            if key.is_empty() || !known {
                                command_result = Err("Unknown app".to_string());
                            } else {
                            let mut next = cfg.clone();
                            let mut changed = match next.updater.set_app_ignored(&key, ignore) {
                                Ok(changed) => changed,
                                Err(e) => {
                                    command_result = Err(e.to_string());
                                    false
                                }
                            };
                            // A blank note means "unchanged" here because ignore toggles
                            // carry one; SetAppNote owns explicit create/update/delete.
                            let n = note.trim();
                            if !n.is_empty() && known {
                                match next.updater.set_app_note(&key, n) {
                                    Ok(note_changed) => changed |= note_changed,
                                    Err(e) => {
                                        warn!("Refusing app note for '{key}': {e}");
                                        command_result = Err(e.to_string());
                                    }
                                }
                            }
                            if command_result.is_ok() && changed {
                                if let Err(e) = config::save_async(&next, "config.toml").await {
                                    warn!("Failed to save app setting: {e}");
                                    command_result =
                                        Err(format!("App setting could not be saved: {e}"));
                                } else {
                                    cfg = next;
                                }
                            }
                            // Reflect the toggle on the live row so the UI shows it
                            // immediately — the broadcast below carries the unchanged
                            // apps list from the last cycle, so without this the
                            // optimistic dim would revert on the next poll.
                            if command_result.is_ok() {
                                for a in st.updater.apps.iter_mut() {
                                    if a.id.eq_ignore_ascii_case(&key) {
                                        a.ignored = ignore;
                                        if !n.is_empty() {
                                            a.note = n.to_string();
                                        }
                                    }
                                }
                                st.updater.app_notes = cfg.updater.note_views();
                                st.updater.settings = cfg.updater.to_view();
                                command_result = Ok("App setting saved".to_string());
                                pipe.broadcast_status(build_status(&st));
                            }
                            }
                        }
                        UiMsg::SetAppNote { id, note } => {
                            let key = id.trim().to_lowercase();
                            let known = st
                                .updater
                                .apps
                                .iter()
                                .any(|a| a.id.eq_ignore_ascii_case(&key))
                                || cfg.updater.notes.contains_key(&key);
                            if !known {
                                warn!("Refusing app note for unknown id '{key}'");
                                command_result = Err("Unknown app".to_string());
                            } else {
                            let mut next = cfg.clone();
                            match next.updater.set_app_note(&key, &note) {
                                Ok(true) => {
                                    if let Err(e) = config::save_async(&next, "config.toml").await {
                                        warn!("Failed to save app note: {e}");
                                        command_result =
                                            Err(format!("App note could not be saved: {e}"));
                                    } else {
                                        cfg = next;
                                        let saved = cfg
                                            .updater
                                            .notes
                                            .get(&key)
                                            .cloned()
                                            .unwrap_or_default();
                                        for app in st.updater.apps.iter_mut() {
                                            if app.id.eq_ignore_ascii_case(&key) {
                                                app.note = saved.clone();
                                            }
                                        }
                                        st.updater.app_notes = cfg.updater.note_views();
                                        command_result = Ok("App note saved".to_string());
                                        pipe.broadcast_status(build_status(&st));
                                    }
                                }
                                Ok(false) => command_result = Ok("App note unchanged".to_string()),
                                Err(e) => {
                                    warn!("Refusing app note for '{key}': {e}");
                                    command_result = Err(e.to_string());
                                }
                            }
                            }
                        }
                        UiMsg::SetAdvisorSettings(update) => {
                            let mut next = cfg.clone();
                            next.advisor.apply_view(*update);
                            if let Err(e) = config::save_async(&next, "config.toml").await {
                                warn!("Failed to save advisor settings: {e}");
                                command_result =
                                    Err(format!("Advisor settings could not be saved: {e}"));
                            } else {
                                cfg = next;
                                let view = cfg.advisor.to_view();
                                match st.advisor.as_mut() {
                                    Some(a) => {
                                        a.enabled = cfg.advisor.enabled;
                                        a.settings = view;
                                    }
                                    None => {
                                        st.advisor = Some(AdvisorStatus {
                                            enabled: cfg.advisor.enabled,
                                            settings: view,
                                            ..Default::default()
                                        });
                                    }
                                }
                                command_result = Ok("Advisor settings saved".to_string());
                                pipe.broadcast_status(build_status(&st));
                            }
                        }
                        UiMsg::UndoRegistry { id } => {
                            match audit::claim_registry_undo(&db, id).await {
                                Ok(Some(undo)) => {
                                    let db_u = db.clone();
                                    let done = undo_done_tx.clone();
                                    tokio::spawn(async move {
                                        let label = format!(
                                            "Undo registry {}\\{}",
                                            undo.key_path, undo.value_name
                                        );
                                        let (success, output) =
                                            match executor::registry::restore_value(&undo).await {
                                                Ok(m) => (true, m),
                                                Err(e) => (false, e.to_string()),
                                            };
                                        // Restore failed → release the claim so it can be
                                        // retried rather than latched as done.
                                        if !success {
                                            if let Err(e) =
                                                audit::unclaim_registry_undo(&db_u, id).await
                                            {
                                                error!("Failed to release registry undo claim: {e}");
                                            }
                                        }
                                        let _ = done.send(ExecOutcome {
                                            key: String::new(),
                                            label: label.clone(),
                                            exec_action: label,
                                            success,
                                            output,
                                            diagnosis: "User-requested undo of a registry change"
                                                .to_string(),
                                            confidence: 1.0,
                                            reason: Some("undo by user".to_string()),
                                            undo_id: None,
                                            // On success, clear this undo_id from the original
                                            // activity row so its "Undo" button disappears.
                                            cleared_undo_id: success.then_some(id),
                                            refresh_pending: false,
                                        });
                                    });
                                    command_result = Ok("Registry undo started".to_string());
                                }
                                Ok(None) => {
                                    warn!("Registry undo {id} not found or already undone");
                                    command_result =
                                        Err("Undo is missing or already claimed".to_string());
                                }
                                Err(e) => {
                                    error!("Failed to load registry undo {id}: {e}");
                                    command_result = Err(format!("Undo could not start: {e}"));
                                }
                            }
                        }
                        UiMsg::AskEir {
                            question,
                            attachments,
                        } => {
                            let now = chrono::Utc::now().timestamp();
                            let rejection = ask::ask_rejection_reason(
                                &question,
                                ai.is_some(),
                                st.ask_running,
                                st.last_ask_at,
                                now,
                            )
                            .or_else(|| ask::attachment_rejection_reason(&attachments));
                            if let Some(reason) = rejection {
                                refresh_ask(&mut st, Some(reason.to_string()));
                                command_result = Err(reason.to_string());
                                pipe.broadcast_status(build_status(&st));
                            } else if let Some(ai_ref) = ai.as_ref() {
                                st.ask_running = true;
                                command_result = Ok("Question accepted".to_string());
                                refresh_ask(&mut st, None);
                                pipe.broadcast_status(build_status(&st));
                                // Route attachments: text files fold into the prompt, images
                                // go to the multimodal call. The tray already read + bounded
                                // them (in the user's session) — here we only classify.
                                let mut text_files: Vec<(String, String)> = Vec::new();
                                let mut images: Vec<ai::client::ImageInput> = Vec::new();
                                let mut labels: Vec<String> = Vec::new();
                                for a in &attachments {
                                    labels.push(a.name.clone());
                                    if a.kind == "image" && !a.content.is_empty() {
                                        images.push(ai::client::ImageInput {
                                            base64: a.content.clone(),
                                            media_type: if a.media_type.is_empty() {
                                                "image/jpeg".to_string()
                                            } else {
                                                a.media_type.clone()
                                            },
                                        });
                                    } else if a.kind == "text" {
                                        text_files.push((a.name.clone(), a.content.clone()));
                                    }
                                }
                                let images_dropped =
                                    !images.is_empty() && !ai_ref.supports_images();
                                let image_count = images.len();
                                // Snapshot the loop-owned context now; gather the DB-derived
                                // trend/learned pieces inside the off-loop task.
                                let (cpu, memory, disk) = (st.cpu, st.memory, st.disk);
                                let failed = st.failed_services.clone();
                                let last_analysis = st.last_analysis.clone();
                                let recent_problems: Vec<String> = st
                                    .recent_problems
                                    .iter()
                                    .rev()
                                    .take(8)
                                    .map(|p| format!("{} ({})", p.diagnosis, p.action))
                                    .collect();
                                let recent_executions: Vec<String> = st
                                    .recent_executions
                                    .iter()
                                    .rev()
                                    .take(8)
                                    .map(|e| {
                                        format!(
                                            "{}: {}",
                                            e.action,
                                            if e.success { "ok" } else { "failed" }
                                        )
                                    })
                                    .collect();
                                let db_a = db.clone();
                                let ai_a = ai_ref.clone();
                                let done = ask_done_tx.clone();
                                // Feed the last few Q&A pairs into the prompt so follow-ups
                                // keep context; clone here because the closure can't borrow `st`.
                                let ask_history: Vec<eir_proto::AskEntry> =
                                    st.ask_entries.iter().cloned().collect();
                                tokio::spawn(async move {
                                    // Bound the whole answer in a nested task + timeout,
                                    // mirroring the analysis/digest tasks, so a wedged AI
                                    // call can't latch ask_running for the process lifetime.
                                    const ASK_MAX: Duration = Duration::from_secs(4 * 60);
                                    let q = question.clone();
                                    let labels_c = labels;
                                    let mut inner = tokio::spawn(async move {
                                        let trend =
                                            audit::metric_trend(&db_a).await.ok().flatten();
                                        let learned = learn::LearnedFacts::load(&db_a)
                                            .await
                                            .prompt_section();
                                        let ctx = ask::AskContext {
                                            cpu,
                                            memory,
                                            disk,
                                            failed_services: failed,
                                            trend,
                                            last_analysis,
                                            recent_problems,
                                            recent_executions,
                                            learned,
                                        };
                                        let attach_section =
                                            ask::format_text_attachments(&text_files);
                                        let prompt =
                                            ask::build_prompt(&ctx, &question, &attach_section, &ask_history);
                                        // Drop images the provider can't read (CLI providers);
                                        // the answer gets a note so the user isn't misled.
                                        let imgs = if images_dropped { Vec::new() } else { images };
                                        ai_a.complete_multimodal(&prompt, &imgs, "")
                                            .await
                                            .map(|(t, u)| {
                                                let mut t = t.trim().to_string();
                                                if images_dropped && !t.is_empty() {
                                                    t = format!(
                                                        "(Note: your AI provider can't read \
                                                         images, so {image_count} attached \
                                                         image(s) weren't analysed.)\n\n{t}"
                                                    );
                                                }
                                                (t, u)
                                            })
                                            .map_err(|e| e.to_string())
                                    });
                                    let result =
                                        match tokio::time::timeout(ASK_MAX, &mut inner).await {
                                            Ok(Ok(Ok((t, u)))) if !t.is_empty() => Ok((t, u)),
                                            Ok(Ok(Ok(_))) => {
                                                Err("the model returned an empty answer".to_string())
                                            }
                                            Ok(Ok(Err(e))) => Err(e),
                                            Ok(Err(_join)) => {
                                                Err("the answer task panicked".to_string())
                                            }
                                            Err(_elapsed) => {
                                                abort_task(inner).await;
                                                Err("answering timed out".to_string())
                                            }
                                        };
                                    let _ = done.send((q, labels_c, result));
                                });
                            }
                        }
                        UiMsg::ClearAsk => {
                            if let Some(reason) = ask::clear_rejection_reason(st.ask_running) {
                                command_result = Err(reason.to_string());
                            } else {
                                // Wipe the in-memory history and reset the spend guard so the
                                // next question starts a fresh conversation.
                                st.ask_entries.clear();
                                st.last_ask_at = 0;
                                refresh_ask(&mut st, None);
                                command_result = Ok("Ask history cleared".to_string());
                                pipe.broadcast_status(build_status(&st));
                            }
                        }
                        UiMsg::ScanDisk => {
                            // Gate on the scan-in-flight flag and pause (a paused guardian
                            // shouldn't be walking the disk). Keep prior results visible
                            // while the new scan runs.
                            if st.paused {
                                command_result =
                                    Err("Resume monitoring before scanning".to_string());
                            } else if st.disk_scan_running {
                                command_result =
                                    Err("A disk scan is already running".to_string());
                            } else {
                                st.disk_scan_running = true;
                                command_result = Ok("Disk scan started".to_string());
                                let scanned_at =
                                    st.disk_insights.as_ref().map(|d| d.scanned_at).unwrap_or(0);
                                let entries = st
                                    .disk_insights
                                    .as_ref()
                                    .map(|d| d.entries.clone())
                                    .unwrap_or_default();
                                st.disk_insights = Some(eir_proto::DiskInsightsView {
                                    running: true,
                                    scanned_at,
                                    error: None,
                                    entries,
                                });
                                pipe.broadcast_status(build_status(&st));
                                let done = disk_done_tx.clone();
                                tokio::spawn(async move {
                                    let _ = done.send(disk_scan::run_scan().await);
                                });
                            }
                        }
                        UiMsg::CleanDiskEntry { id } => {
                            // Map the opaque id to a safe action from THIS service's own
                            // last scan (unknown/stale id → ignore), then route it through
                            // the normal policy gate. The wire never carries an action.
                            match st.disk_targets.get(&id).cloned() {
                                Some(action) => {
                                    command_result = route_user_action(
                                        &mut st,
                                        &pol,
                                        &db,
                                        &exec_tx,
                                        action,
                                        "User-requested disk cleanup",
                                        "user-requested cleanup",
                                        false,
                                    )
                                    .await;
                                    pipe.broadcast_status(build_status(&st));
                                }
                                None => {
                                    info!(%id, "Clean requested for unknown/stale disk entry — ignoring");
                                    command_result =
                                        Err("Disk entry is stale; scan again".to_string());
                                }
                            }
                        }
                        UiMsg::ScanStartup => {
                            let active_sid = executor::startup::active_user_sid().ok();
                            if clear_stale_startup_cache(&mut st, active_sid.as_deref()) {
                                pipe.broadcast_status(build_status(&st));
                            }
                            if st.paused {
                                command_result =
                                    Err("Resume monitoring before scanning".to_string());
                            } else if active_sid.is_none() {
                                command_result =
                                    Err("Startup scan requires an active interactive user".to_string());
                            } else if st.startup_scan_running {
                                command_result =
                                    Err("A startup scan is already running".to_string());
                            } else {
                                let scan_sid = active_sid.unwrap_or_default();
                                st.startup_owner_sid = Some(scan_sid.clone());
                                st.startup_scan_running = true;
                                command_result = Ok("Startup scan started".to_string());
                                let scanned_at =
                                    st.startup.as_ref().map(|s| s.scanned_at).unwrap_or(0);
                                let entries = st
                                    .startup
                                    .as_ref()
                                    .map(|s| s.entries.clone())
                                    .unwrap_or_default();
                                st.startup = Some(eir_proto::StartupView {
                                    running: true,
                                    scanned_at,
                                    error: None,
                                    entries,
                                });
                                pipe.broadcast_status(build_status(&st));
                                let done = startup_done_tx.clone();
                                let ai_s = ai.clone();
                                let model_s = cfg.api.update_check_model.clone();
                                tokio::spawn(async move {
                                    let result =
                                        startup_scan::run_scan(ai_s, model_s, scan_sid.clone())
                                            .await;
                                    let _ = done.send((scan_sid, result));
                                });
                            }
                        }
                        UiMsg::SetStartupEntry { id, enable } => {
                            // Reconstruct the toggle from THIS service's own last scan
                            // (unknown/stale id → ignore), then route it through the normal
                            // policy gate with approval forced. The cache is SID-owned:
                            // after fast-user-switching, old opaque ids are invalid.
                            let active_sid = executor::startup::active_user_sid().ok();
                            let owner_current = startup_owner_matches(
                                st.startup_owner_sid.as_deref(),
                                active_sid.as_deref(),
                            );
                            let cache_cleared = if owner_current {
                                false
                            } else {
                                clear_stale_startup_cache(&mut st, active_sid.as_deref())
                            };
                            let target = owner_current
                                .then(|| {
                                    st.startup_targets.get(&id).map(|t| {
                                        (
                                            t.name.clone(),
                                            t.location.clone(),
                                            t.hive.clone(),
                                            t.task_path.clone(),
                                        )
                                    })
                                })
                                .flatten();
                            match target {
                                Some((name, location, hive, task_path)) => {
                                    let name = if location == "scheduled_task" {
                                        task_path
                                    } else {
                                        name
                                    };
                                    let action = FixAction::StartupSet {
                                        name,
                                        location,
                                        hive,
                                        enable,
                                    };
                                    let diag = if enable {
                                        "User re-enabled a startup entry"
                                    } else {
                                        "User disabled a startup entry"
                                    };
                                    command_result = route_user_action(
                                        &mut st,
                                        &pol,
                                        &db,
                                        &exec_tx,
                                        action,
                                        diag,
                                        "user startup change",
                                        true,
                                    )
                                    .await;
                                    pipe.broadcast_status(build_status(&st));
                                }
                                None => {
                                    info!(%id, "Startup toggle for unknown/stale entry — ignoring");
                                    command_result =
                                        Err("Startup entry is stale; scan again".to_string());
                                    if cache_cleared {
                                        pipe.broadcast_status(build_status(&st));
                                    }
                                }
                            }
                        }
                        }
                        pipe.command_result(request_id, command_result);
                        continue;
                    }
                }
                cycle_count += 1;
                last_cycle_at = tokio::time::Instant::now();
                // A scheduled tick covers any pending reaction — cancel it so the same
                // signals aren't analysed twice back-to-back. BUT if an analysis is still
                // in flight this pass will bail below without analysing, so keep the
                // reaction pending — otherwise a tick landing mid-analysis would silently
                // drop it and defer the reactive fast-path to the next full tick. The
                // sleep_until arm is gated on !analysis_running, so a preserved past
                // deadline won't busy-loop; it fires once the analysis completes.
                if !analysis_running {
                    react_at = None;
                }

                // Re-discover log directories every 20 cycles
                if cycle_count.is_multiple_of(20) {
                    let extra = cfg.monitoring.log_directories.clone();
                    if let Ok(Some((_owner_sid, all))) = tokio::task::spawn_blocking(move || {
                        discover_watch_dirs_for_stable_user(&extra)
                    })
                    .await
                    {
                        let replacement: HashSet<PathBuf> = all.iter().cloned().collect();
                        let added = replacement.difference(&known_watch_dirs).count();
                        let removed = known_watch_dirs.difference(&replacement).count();
                        if dir_update_tx.send(all).is_ok() {
                            known_watch_dirs = replacement;
                            if added > 0 || removed > 0 {
                                info!(
                                    added,
                                    removed, "Reconciled discovered log directories"
                                );
                            }
                        }
                    }
                }

                // ── Game Mode lease reconcile ─────────────────────────────────
                // If the lease lapsed with no explicit off (e.g. the tray crashed), restore
                // any boosted power plan and re-analyse — mirroring the SetGaming end-edge.
                // restore() is a cheap DB read that no-ops without a stored boost. Bounds
                // the "boosted after a tray crash" window to one decision tick.
                {
                    let g = is_gaming(&st);
                    if was_gaming && !g {
                        let _ = game_mode_edge_tx.send((false, false));
                        last_fingerprint = None;
                    }
                    was_gaming = g;
                }

                // ── Autonomous updater: start a scheduled cycle when due ──────
                {
                    let now = chrono::Utc::now().timestamp();
                    let due = updater_due(
                        cfg.updater.enabled,
                        cfg.updater.schedule_interval_secs as i64,
                        st.updater.last_run,
                        &st,
                        now,
                    );
                    if due {
                        info!("Autonomous update cycle due — starting");
                        st.updater_running = true;
                        st.updater.running = true;
                        st.updater.enabled = true;
                        st.updater.phase = "checking…".to_string();
                        pipe.broadcast_status(build_status(&st));
                        spawn_update_cycle(
                            &cfg,
                            &db,
                            &update_done_tx,
                            &update_progress_tx,
                            None,
                        );
                    }
                }

                if st.paused {
                    continue;
                }

                // ── Collect signals ──────────────────────────────────────────
                let history =
                    audit::get_recent_decisions(&db, 5).await.unwrap_or_else(|e| {
                        warn!("Failed to load decision history: {e}");
                        vec![]
                    });

                let snapshot = SignalSnapshot {
                    timestamp:        chrono::Utc::now(),
                    event_log:        signals::event_log::drain(&event_log_shared),
                    file_changes:     signals::file_watch::drain(&file_watch_shared),
                    system_state:     signals::wmi::current(&wmi_shared),
                    decision_history: history.clone(),
                };

                info!(
                    event_entries = snapshot.event_log.len(),
                    file_changes  = snapshot.file_changes.len(),
                    "Signal snapshot collected"
                );

                // ── Update metrics in broadcast ──────────────────────────────
                apply_live_metrics(&mut st, &snapshot.system_state);
                // Refresh the dashboard resource timeline (last 24h, thinned) once per
                // tick — cheap, and off the per-broadcast path (build_status only clones
                // the cached vec). Reads the same per-cycle series the trend detector uses.
                {
                    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
                    match audit::metric_history(&db, &cutoff, 200).await {
                        Ok(h) => st.history = h,
                        Err(e) => warn!("Failed to load metric history: {e}"),
                    }
                }
                pipe.broadcast_status(build_status(&st));

                // No AI provider configured — keep collecting signals and serving
                // the UI (so Settings stays usable), but skip analysis.
                let Some(ai) = ai.as_ref() else {
                    continue;
                };

                // Weekly health digest: time-based (checked here, before the idle-skip
                // gate, so a quiet week still gets one), off-loop so the AI call never
                // blocks the loop. One at a time.
                let now_ts = chrono::Utc::now().timestamp();
                if !st.digest_running
                    && !is_gaming_at(&st, now_ts)
                    && (st.last_digest_at == 0 || now_ts - st.last_digest_at >= DIGEST_INTERVAL_SECS)
                {
                    st.digest_running = true;
                    let db_d = db.clone();
                    let ai_d = ai.clone();
                    let model_d = cfg.api.update_check_model.clone();
                    let done_d = digest_done_tx.clone();
                    let db_save = db_d.clone();
                    tokio::spawn(async move {
                        // Bound the whole generation in a nested task + timeout, mirroring
                        // the analysis/label/exec tasks, so a wedged AI call can't latch
                        // digest_running for the process lifetime.
                        const DIGEST_MAX: Duration = Duration::from_secs(6 * 60);
                        let mut inner =
                            tokio::spawn(
                                async move { digest::generate(&db_d, &ai_d, &model_d).await },
                            );
                        let result = match tokio::time::timeout(DIGEST_MAX, &mut inner).await {
                            Ok(Ok(r)) => r,
                            Ok(Err(_join)) => Err(anyhow::anyhow!("digest task panicked")),
                            Err(_elapsed) => {
                                abort_task(inner).await;
                                Err(anyhow::anyhow!("digest generation timed out"))
                            }
                        };
                        match result {
                            Ok((text, usage)) => {
                                if let Err(e) = audit::save_digest(&db_save, &text).await {
                                    warn!("Failed to save health digest: {e}");
                                }
                                let view = eir_proto::DigestView {
                                    text,
                                    generated_at: chrono::Utc::now().timestamp(),
                                };
                                let _ = done_d.send((view, usage));
                            }
                            Err(e) => {
                                // Report failure with an empty text + a real timestamp: the
                                // arm clears digest_running and pushes the next attempt out a
                                // week (no retry storm), without overwriting a prior digest.
                                warn!("Health digest generation failed: {e}");
                                let _ = done_d.send((
                                    eir_proto::DigestView {
                                        text: String::new(),
                                        generated_at: chrono::Utc::now().timestamp(),
                                    },
                                    None,
                                ));
                            }
                        }
                    });
                }

                if analysis_running {
                    info!("Analysis already in flight - skipping this pass");
                    st.status = resting_status(&st);
                    if !st.paused {
                        st.error = None;
                    }
                    pipe.broadcast_status(build_status(&st));
                    continue;
                }

                // ── Feedback after-states ────────────────────────────────────
                if let Err(e) =
                    feedback::update_after_states(&db, &snapshot.system_state).await
                {
                    warn!("Feedback update failed: {e}");
                }

                // Retention: the detectors only look back ~30 days, so keep the
                // feedback/rejection tables from growing without bound (cheap once
                // they're pruned — later cycles match nothing).
                const RETENTION_DAYS: i64 = 90;
                if let Err(e) = feedback::prune_old(&db, RETENTION_DAYS).await {
                    warn!("Feedback prune failed: {e}");
                }
                if let Err(e) = learn::prune_old_rejections(&db, RETENTION_DAYS).await {
                    warn!("Rejection prune failed: {e}");
                }
                // The decisions / system_state_history / execution_log tables have no other
                // retention; prune them on the same window (readers need only 24h / a few rows).
                if let Err(e) = audit::prune_old(&db, RETENTION_DAYS).await {
                    warn!("Audit prune failed: {e}");
                }
                let feedback_summary =
                    feedback::recent_summary(&db, 10).await.unwrap_or_default();

                // Tier-2 (optional): give one not-yet-explained learned fact a one-sentence
                // human-readable AI explanation (text only — never changes behaviour). It's
                // an AI call, so run it OFF the loop like the analysis — inline it could block
                // ui_rx (Approve/Reject/Pause) for up to the provider timeout. label_one skips
                // the AI call entirely when every fact is already labelled, and the flag stops
                // it stacking; the result is picked up by the next facts_for_view refresh.
                if !labelling.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let db_l = db.clone();
                    let ai_l = ai.clone();
                    let model_l = cfg.api.update_check_model.clone();
                    let flag_l = labelling.clone();
                    tokio::spawn(async move {
                        // Reset via a drop-guard so an unlikely panic in label_one still
                        // clears the flag (panic = unwind here) — otherwise the labeller
                        // would latch off for the rest of the process, like the executor
                        // and analysis tasks guard against their own failure paths.
                        struct ResetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
                        impl Drop for ResetOnDrop {
                            fn drop(&mut self) {
                                self.0.store(false, std::sync::atomic::Ordering::Release);
                            }
                        }
                        let _reset = ResetOnDrop(flag_l);
                        // A hang (not a panic) never drops the guard on its own, so bound
                        // it: run label_one in a nested task under a timeout and abort on
                        // elapse — mirrors the analysis task, so a wedged AI call can't
                        // latch the labeller off for the rest of the process lifetime.
                        let (db_i, ai_i, model_i) = (db_l.clone(), ai_l.clone(), model_l.clone());
                        let mut inner = tokio::spawn(async move {
                            learn::label_one(&db_i, &ai_i, &model_i).await;
                        });
                        if tokio::time::timeout(LABEL_MAX, &mut inner).await.is_err() {
                            abort_task(inner).await;
                            warn!(
                                "labeller exceeded {}m and was stopped",
                                LABEL_MAX.as_secs() / 60
                            );
                        }
                    });
                }

                // Refresh the "what Eir has learned" card every cycle (incl. idle ones) so
                // it reflects facts formed by both the decision loop and the updater.
                if let Ok(facts) = learn::facts_for_view(&db).await {
                    st.learned_facts = facts;
                }

                // ── Decide whether to call the AI ─────────────────────────────
                // Skip benign/unchanged idle cycles to save usage — but always run
                // the first analysis, on any actionable change, and on a periodic
                // heartbeat, so the UI shows a current result even on a healthy box.
                let fingerprint = actionable_fingerprint(&snapshot);
                let changed =
                    fingerprint.is_some() && fingerprint.as_deref() != last_fingerprint.as_deref();
                let heartbeat_due = last_analysis_at
                    .map(|t| t.elapsed() >= ANALYSIS_HEARTBEAT)
                    .unwrap_or(true);
                if !changed && !heartbeat_due {
                    // State went quiet (nothing actionable): forget the last fingerprint
                    // so the SAME problem recurring later counts as a change instead of
                    // being suppressed until the multi-hour heartbeat.
                    if fingerprint.is_none() {
                        last_fingerprint = None;
                    }
                    info!("No actionable change since last analysis — skipping");
                    // resting_status keeps a still-running off-loop fix as "Executing"
                    // (and an outstanding approval as "PendingApproval") instead of
                    // flipping the UI to "Active" mid-action.
                    st.status = resting_status(&st);
                    if !st.paused {
                        st.error = None;
                    }
                    pipe.broadcast_status(build_status(&st));
                    continue;
                }

                // ── Self-improvement: learn issue-side patterns, then apply them ──
                // Derive ineffective-fix / rejected-action facts from history, then load
                // the in-force facts so issue analysis reasons with them (prompt) and the
                // confidence gate accounts for them (per-problem routing below).
                learn::analyse_issues(&db).await;
                let learned_facts = learn::LearnedFacts::load(&db).await;
                // Fold a resource-trend note (from the previously-unused
                // system_state_history series) into the read-only context so the AI can
                // see a slow climb — disk filling, sustained CPU/memory rise — that a
                // single snapshot can't reveal.
                let learned_section = match (
                    learned_facts.prompt_section(),
                    audit::metric_trend(&db).await.ok().flatten(),
                ) {
                    (Some(l), Some(t)) => Some(format!("{l}\n\n{t}")),
                    (Some(l), None) => Some(l),
                    (None, Some(t)) => Some(t),
                    (None, None) => None,
                };

                // ── Claude analysis (off-loop) ────────────────────────────────
                // Day-rollover reset for the advisor's daily counters - must happen
                // before handing baselines to the off-loop analysis task.
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                if st.advisor_spend_date != today {
                    st.advisor_spend_date = today;
                    st.advisor_spent_today = 0.0;
                    st.advisor_escalations_today = 0;
                }

                analysis_running = true;
                let ai_task = ai.clone();
                let advisor_cfg = cfg.advisor.clone();
                let spent_baseline = st.advisor_spent_today;
                let escalations_baseline = st.advisor_escalations_today;
                let tx = analysis_done_tx.clone();
                tokio::spawn(async move {
                    let mut inner = tokio::spawn(async move {
                        match ai_task
                            .analyze(&snapshot, &history, Some(&feedback_summary), learned_section.as_deref())
                            .await
                        {
                            Ok((decision, usage)) => {
                                let mut usages = Vec::new();
                                usages.extend(usage);
                                let mut adv = AdvisorStatus {
                                    enabled: advisor_cfg.enabled,
                                    escalated: false,
                                    escalation_model: String::new(),
                                    reason: String::new(),
                                    spent_today_usd: spent_baseline,
                                    settings: advisor_cfg.to_view(),
                                };
                                let mut spent = spent_baseline;
                                let mut escalations = escalations_baseline;
                                let mut final_decision = decision;
                                if let Some(reason) =
                                    should_escalate(&final_decision, &advisor_cfg, spent, escalations)
                                {
                                    info!(reason, "Advisor escalating to a deeper analysis pass");
                                    match ai_task
                                        .analyze_with(
                                            &snapshot,
                                            &history,
                                            Some(&feedback_summary),
                                            learned_section.as_deref(),
                                            Some(advisor_cfg.escalation_model.as_str()),
                                            Some(advisor_cfg.escalation_effort.as_str()),
                                        )
                                        .await
                                    {
                                        Ok((d2, usage2)) => {
                                            // Count only a SUCCESSFUL escalation against the
                                            // daily cap — a failed pass produced no deeper
                                            // analysis, so it shouldn't burn a slot.
                                            escalations += 1;
                                            if let Some(u) = &usage2 { spent += u.cost_usd; }
                                            usages.extend(usage2);
                                            final_decision = d2;
                                            adv.escalated = true;
                                            adv.reason = reason.to_string();
                                            adv.escalation_model = advisor_cfg.escalation_model.clone();
                                            adv.spent_today_usd = spent;
                                        }
                                        Err(e) => warn!("Advisor escalation failed: {e}"),
                                    }
                                }
                                Ok(AnalysisSuccess {
                                    snapshot,
                                    fingerprint,
                                    decision: final_decision,
                                    usage: usages,
                                    advisor: adv,
                                    advisor_spent_today: spent,
                                    advisor_escalations_today: escalations,
                                    escalation_cost_usd: spent - spent_baseline,
                                })
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    });
                    let outcome = match tokio::time::timeout(ANALYSIS_MAX, &mut inner).await {
                        Ok(Ok(result)) => result,
                        Ok(Err(_join)) => Err("analysis task panicked".to_string()),
                        Err(_elapsed) => {
                            abort_task(inner).await;
                            Err(format!(
                                "analysis exceeded {}m and was stopped",
                                ANALYSIS_MAX.as_secs() / 60
                            ))
                        }
                    };
                    let _ = tx.send(outcome).await;
                });
                continue;
            }
        } => {}
        _ = shutdown => {
            info!("Shutdown signal received — stopping service loop");
        }
    }

    // Cancel the serial power-plan worker before restoring. If it were left detached,
    // a queued boost could run after shutdown restoration and leave High Performance
    // active indefinitely (especially on uninstall, when there may be no next startup).
    drop(game_mode_edge_tx);
    game_mode_edge_handle.abort();
    let _ = game_mode_edge_handle.await;
    game_mode::restore(&db).await;

    // Drain the executor before the runtime is dropped. Both exit paths land here: a
    // settings-save `restart_self()` returns out of the loop block, and a shutdown/SCM
    // stop (including the self-updater's `sc stop`) cancels it. An approved fix that was
    // queued or executing gets to finish and log (execution_log / undo snapshot / feedback)
    // instead of being aborted when the runtime drops — PROVIDED it completes within the
    // drain window. Dropping the only `ExecJob` sender closes the job channel so the worker
    // stops once its queue is empty; the idle case joins instantly. The 20s cap plus
    // Game Mode's bounded 10s restore stays inside SCM's 35s StopPending hint. A genuinely
    // long repair (SFC/DISM, tens of minutes) can still be cut off by SCM force-killing
    // the process, which no in-process drain can outlast — those remain best-effort.
    drop(exec_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(20), exec_handle).await;
    info!("Executor drained — service loop stopped");
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn abort_task_waits_until_the_cancelled_future_is_dropped() {
        struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped_in_task = dropped.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _dropped = Dropped(dropped_in_task);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("task started");

        abort_task(handle).await;

        assert!(
            dropped.load(std::sync::atomic::Ordering::Acquire),
            "abort completion must be observed before in-flight state is released"
        );
    }

    /// resting_status must order Paused > Gaming > PendingApproval > Executing > Active.
    #[test]
    fn resting_status_precedence() {
        let mut st = SvcState::default();
        assert_eq!(resting_status(&st), "Active");

        st.in_flight.insert("ServiceRestart".into());
        assert_eq!(resting_status(&st), "Executing");

        // Game Mode outranks an in-flight execution / pending approval.
        st.gaming_until = i64::MAX;
        assert_eq!(resting_status(&st), "Gaming");

        // Paused outranks Game Mode (an explicit user pause wins).
        st.paused = true;
        assert_eq!(resting_status(&st), "Paused");
    }

    #[test]
    fn startup_cache_is_cleared_when_the_active_sid_changes() {
        let owner = "S-1-5-21-1-2-3-1001";
        let mut st = SvcState {
            startup: Some(eir_proto::StartupView::default()),
            startup_owner_sid: Some(owner.to_string()),
            startup_scan_running: true,
            ..Default::default()
        };
        st.startup_targets.insert(
            "task".to_string(),
            startup_scan::StartupToggle {
                name: "Updater".to_string(),
                location: "scheduled_task".to_string(),
                hive: owner.to_string(),
                task_path: r"\Vendor\Updater".to_string(),
            },
        );

        assert!(!clear_stale_startup_cache(&mut st, Some(owner)));
        assert!(st.startup.is_some());
        assert_eq!(st.startup_targets.len(), 1);

        assert!(clear_stale_startup_cache(
            &mut st,
            Some("S-1-5-21-9-8-7-1002")
        ));
        assert!(st.startup.is_none());
        assert!(st.startup_owner_sid.is_none());
        assert!(!st.startup_scan_running);
        assert!(st.startup_targets.is_empty());
    }

    #[test]
    fn watcher_roots_fail_closed_before_user_switch_discovery() {
        let old = "S-1-5-21-1-2-3-1001";
        let new = "S-1-5-21-9-8-7-1002";
        assert!(watch_roots_need_clearing(Some(old), Some(new)));
        assert!(watch_roots_need_clearing(Some(old), None));
        assert!(!watch_roots_need_clearing(Some(old), Some(old)));
        assert!(!watch_roots_need_clearing(None, Some(new)));
    }

    /// Game Mode is a lease: active only while `gaming_until > now`, so a crashed/closed
    /// tray auto-expires it.
    #[test]
    fn gaming_is_a_lease() {
        let mut st = SvcState::default();
        assert!(!is_gaming_at(&st, 1000));
        st.gaming_until = 1090; // e.g. now(1000) + 90s lease
        assert!(is_gaming_at(&st, 1000));
        assert!(is_gaming_at(&st, 1089));
        assert!(!is_gaming_at(&st, 1090)); // expired at the deadline
        assert!(!is_gaming_at(&st, 2000));

        // The manual latch keeps gaming active regardless of the (expired) lease — the two
        // inputs are independent (auto detector = lease, user toggle = latch).
        st.gaming_manual = true;
        assert!(is_gaming_at(&st, 5000));
        st.gaming_manual = false;
        st.gaming_until = 0;
        assert!(!is_gaming_at(&st, 5000));
    }

    /// The user's manual OFF must end Game Mode even while the auto lease is live —
    /// otherwise the sidebar toggle silently does nothing mid-game (the click only
    /// cleared the latch and the lease kept gaming on).
    #[test]
    fn manual_off_withdraws_the_auto_lease() {
        let mut st = SvcState::default();
        let now = 1_000i64;
        // Auto lease live, no latch: gaming is on.
        apply_set_gaming(&mut st, true, false, now, 90);
        assert!(is_gaming_at(&st, now));
        // Manual OFF ends it now — both inputs cleared.
        apply_set_gaming(&mut st, false, true, now, 90);
        assert!(!is_gaming_at(&st, now));
        assert_eq!(st.gaming_until, 0);

        // Manual ON is a latch that outlives an auto off…
        apply_set_gaming(&mut st, true, true, now, 90);
        apply_set_gaming(&mut st, false, false, now, 90);
        assert!(is_gaming_at(&st, now));
        // …and an auto off never touches the latch.
        assert!(st.gaming_manual);
    }

    /// A scheduled update cycle is suppressed while gaming (but the manual RunUpdatesNow
    /// path — which never calls updater_due — is unaffected).
    #[test]
    fn updater_due_suppressed_during_gaming() {
        let mut st = SvcState::default();
        let now = 100_000i64;
        // Enabled, never run before, not paused/running, not gaming → due.
        assert!(updater_due(true, 3600, 0, &st, now));
        // Gaming → not due.
        st.gaming_until = now + 90;
        assert!(!updater_due(true, 3600, 0, &st, now));
        // Lease expired → due again.
        assert!(updater_due(true, 3600, 0, &st, now + 200));
        // Paused or interval-not-elapsed also suppress (unchanged behaviour).
        st.gaming_until = 0;
        st.paused = true;
        assert!(!updater_due(true, 3600, 0, &st, now));
        st.paused = false;
        assert!(!updater_due(true, 3600, now, &st, now)); // just ran
    }

    #[test]
    fn updater_due_handles_corrupt_extreme_timestamps() {
        let st = SvcState::default();
        assert!(updater_due(true, 3600, i64::MIN, &st, 1));
        assert!(!updater_due(true, 3600, i64::MAX, &st, 1));
    }

    #[test]
    fn targeted_retry_accepts_only_the_named_failed_row() {
        let apps = vec![
            eir_proto::UpdaterAppRow {
                id: "failed tool".into(),
                state: "failed".into(),
                ..Default::default()
            },
            eir_proto::UpdaterAppRow {
                id: "current tool".into(),
                state: "current".into(),
                ..Default::default()
            },
        ];
        assert!(failed_update_for_retry(&apps, "FAILED TOOL").is_some());
        assert!(failed_update_for_retry(&apps, "current tool").is_none());
        assert!(failed_update_for_retry(&apps, "unknown").is_none());
        assert!(failed_update_for_retry(&apps, "bad\nid").is_none());
    }

    /// F1 regression: a manual RefreshStatus that clears a recovered service must survive
    /// the NEXT decision tick. A tick calls `apply_live_metrics(&mut st, wmi::current(...))`,
    /// so if the refresh updated `st.failed_services` but not the WMI cache, the tick reads
    /// the stale cache and reverts the clear. This exercises that exact seam.
    #[test]
    fn manual_refresh_survives_a_following_tick() {
        use crate::signals::wmi;
        use std::sync::{Arc, Mutex};

        // Stale, pre-recovery state: both the cache and st show the service as failed.
        let cache: wmi::SharedState = Arc::new(Mutex::new(Some(SystemState {
            failed_services: vec!["Spooler".into()],
            ..Default::default()
        })));
        let mut st = SvcState {
            failed_services: vec!["Spooler".into()],
            ..Default::default()
        };

        // Manual refresh: the service recovered. The handler clears st AND (the F1 fix)
        // writes the fresh result into the WMI cache. Model both writes.
        let fresh: Vec<String> = vec![];
        st.failed_services = fresh.clone();
        if let Some(s) = cache.lock().unwrap().as_mut() {
            s.failed_services = fresh.clone();
        }

        // A following decision tick re-reads the cache and applies live metrics.
        apply_live_metrics(&mut st, &wmi::current(&cache));

        assert!(
            st.failed_services.is_empty(),
            "a manual refresh must not be reverted by the next tick — if this fails, the \
             RefreshStatus handler stopped updating the WMI cache (F1 regression)"
        );
    }

    /// A stale cache that was NOT reconciled by a refresh still wins on the next tick — the
    /// negative of the above, documenting that `apply_live_metrics` is authoritative from
    /// the cache (so the F1 fix's cache write is load-bearing, not incidental).
    #[test]
    fn tick_applies_the_cache_verbatim() {
        let sys = SystemState {
            collected_at: 123,
            collector_errors: vec!["disk".into()],
            cpu_usage_percent: 12.5,
            failed_services: vec!["W32Time".into()],
            ..Default::default()
        };
        let mut st = SvcState::default();
        apply_live_metrics(&mut st, &sys);
        assert_eq!(st.cpu, 12.5);
        assert_eq!(st.failed_services, vec!["W32Time".to_string()]);
        assert_eq!(st.signals_at, 123);
        assert_eq!(st.signal_errors, vec!["disk".to_string()]);
    }

    /// build_status must project the live metrics + failed services into the broadcast
    /// payload — catches a field added to SvcState but not wired into StatusPayload.
    #[test]
    fn build_status_projects_metrics_and_failed_services() {
        let st = SvcState {
            cpu: 42.0,
            memory: 71.0,
            disk: 55.0,
            failed_services: vec!["Spooler".into(), "W32Time".into()],
            signals_at: 123,
            signal_errors: vec!["memory".into()],
            ..Default::default()
        };
        let payload = build_status(&st);
        assert_eq!(payload.cpu, 42.0);
        assert_eq!(payload.memory, 71.0);
        assert_eq!(payload.disk, 55.0);
        assert_eq!(
            payload.failed_services,
            vec!["Spooler".to_string(), "W32Time".to_string()]
        );
        assert_eq!(payload.signals_at, 123);
        assert_eq!(payload.signal_errors, vec!["memory".to_string()]);
        assert_eq!(
            payload.svc_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}

enum ApprovalPreflightError {
    Resolved(String),
    Retry(String),
}

async fn approved_action_ready(
    pol: &policy::ExecutionPolicy,
    db: &SqlitePool,
    approval: &PendingApproval,
) -> Result<(), ApprovalPreflightError> {
    if let policy::Verdict::Block(reason) = pol.evaluate(&approval.action, approval.info.confidence)
    {
        return Err(ApprovalPreflightError::Resolved(format!(
            "Blocked by current policy: {reason}"
        )));
    }
    match safety::rate_limited(db, &approval.action, pol.execution.rate_limit_mins).await {
        Ok(false) => Ok(()),
        Ok(true) => Err(ApprovalPreflightError::Resolved(
            "Already completed or repeatedly failed recently".to_string(),
        )),
        Err(e) => Err(ApprovalPreflightError::Retry(format!(
            "Safety preflight failed: {e}"
        ))),
    }
}

#[cfg(test)]
mod advisor_tests {
    use super::*;
    use config::AdvisorConfig;
    use models::{ClaudeDecision, Problem};

    fn cfg(enabled: bool) -> AdvisorConfig {
        AdvisorConfig {
            enabled,
            escalation_model: "opus".into(),
            escalation_effort: String::new(),
            low_confidence_threshold: 0.6,
        }
    }

    fn decision(needs_deeper: bool, confidences: &[f32]) -> ClaudeDecision {
        ClaudeDecision {
            analysis: String::new(),
            needs_deeper_analysis: needs_deeper,
            problems: confidences
                .iter()
                .map(|&c| Problem {
                    diagnosis: String::new(),
                    root_cause: String::new(),
                    confidence: c,
                    proposed_fix: serde_json::Value::Null,
                    reasoning: String::new(),
                    side_effects: String::new(),
                    undo_instructions: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn escalates_on_ai_flag_and_low_confidence_only_when_enabled() {
        // Disabled -> never.
        assert!(should_escalate(&decision(true, &[]), &cfg(false), 0.0, 0).is_none());
        // AI flag -> escalate.
        assert!(should_escalate(&decision(true, &[]), &cfg(true), 0.0, 0).is_some());
        // Low-confidence reported problem -> escalate.
        assert!(should_escalate(&decision(false, &[0.4]), &cfg(true), 0.0, 0).is_some());
        // Confident, no flag -> don't.
        assert!(should_escalate(&decision(false, &[0.95]), &cfg(true), 0.0, 0).is_none());
        // Healthy (no problems, no flag) -> don't.
        assert!(should_escalate(&decision(false, &[]), &cfg(true), 0.0, 0).is_none());
    }

    #[test]
    fn count_and_missing_tier_block_escalation() {
        // At the per-day escalation COUNT cap -> don't (the provider-agnostic backstop,
        // even though spend is 0, e.g. a provider that reports no cost).
        assert!(should_escalate(
            &decision(true, &[]),
            &cfg(true),
            0.0,
            MAX_ESCALATIONS_PER_DAY
        )
        .is_none());
        // No escalation tier configured -> don't.
        let mut no_tier = cfg(true);
        no_tier.escalation_model = String::new();
        no_tier.escalation_effort = String::new();
        assert!(should_escalate(&decision(true, &[]), &no_tier, 0.0, 0).is_none());
    }
}

#[cfg(test)]
mod analysis_outcome_tests {
    use super::*;
    use models::ClaudeDecision;

    fn snapshot() -> SignalSnapshot {
        SignalSnapshot {
            timestamp: chrono::Utc::now(),
            event_log: vec![],
            file_changes: vec![],
            system_state: SystemState {
                collected_at: 0,
                collector_errors: vec![],
                uptime_secs: 0,
                cpu_usage_percent: 0.0,
                memory_usage_percent: 0.0,
                memory_available_gb: 0.0,
                disk_usage_percent: 0.0,
                disk_free_gb: 0.0,
                running_services_count: 0,
                failed_services: vec![],
                network_interfaces: vec![],
                network_errors: 0,
                disk_health: String::new(),
                windows_update_status: String::new(),
                security: Default::default(),
            },
            decision_history: vec![],
        }
    }

    /// The channel carries `Result<AnalysisSuccess, String>` straight from the
    /// off-loop analysis task (see the analysis_done_rx arm) — both variants must
    /// carry everything the arm destructures without a placeholder/dummy snapshot.
    #[test]
    fn analysis_success_round_trips_ok_and_err() {
        let ok: Result<AnalysisSuccess, String> = Ok(AnalysisSuccess {
            snapshot: snapshot(),
            fingerprint: Some("F|x|1|0".to_string()),
            decision: ClaudeDecision {
                analysis: "looks fine".into(),
                problems: vec![],
                needs_deeper_analysis: false,
            },
            usage: vec![CallUsage::default()],
            advisor: AdvisorStatus::default(),
            advisor_spent_today: 1.23,
            advisor_escalations_today: 2,
            escalation_cost_usd: 0.0,
        });
        match ok {
            Ok(s) => {
                assert_eq!(s.fingerprint.as_deref(), Some("F|x|1|0"));
                assert_eq!(s.decision.analysis, "looks fine");
                assert_eq!(s.usage.len(), 1);
                assert_eq!(s.advisor_escalations_today, 2);
            }
            Err(_) => panic!("expected Ok"),
        }

        let err: Result<AnalysisSuccess, String> =
            Err("analysis exceeded 10m and was stopped".into());
        match err {
            Ok(_) => panic!("expected Err"),
            Err(e) => assert!(e.contains("exceeded")),
        }
    }
}
