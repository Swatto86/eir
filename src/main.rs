mod ai;
mod audit;
mod config;
mod executor;
mod feedback;
mod models;
mod policy;
mod safety;
mod signals;

use models::SignalSnapshot;
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Manager, State,
};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone, serde::Serialize)]
pub struct ProblemSummary {
    pub diagnosis: String,
    pub confidence: f32,
    pub action: String,
    pub blocked: bool,
    pub auto_executed: bool,
}

#[derive(Clone, serde::Serialize)]
pub struct ExecutionSummary {
    pub action: String,
    pub success: bool,
    pub preview: String,
}

#[derive(Clone, serde::Serialize)]
pub struct ApprovalInfo {
    pub diagnosis: String,
    pub root_cause: String,
    pub confidence: f32,
    pub action: String,
    pub reason: String,
    pub side_effects: String,
    pub undo_instructions: String,
}

pub struct SentryState {
    pub status: String,
    pub paused: bool,
    pub cpu: f32,
    pub memory: f32,
    pub disk: f32,
    pub failed_services: Vec<String>,
    pub last_analysis: String,
    pub recent_problems: VecDeque<ProblemSummary>,
    pub recent_executions: VecDeque<ExecutionSummary>,
    pub pending_approval_info: Option<ApprovalInfo>,
    pub pending_approval_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    pub error: Option<String>,
}

impl Default for SentryState {
    fn default() -> Self {
        Self {
            status: "Initializing".to_string(),
            paused: false,
            cpu: 0.0,
            memory: 0.0,
            disk: 0.0,
            failed_services: vec![],
            last_analysis: String::new(),
            recent_problems: VecDeque::new(),
            recent_executions: VecDeque::new(),
            pending_approval_info: None,
            pending_approval_tx: None,
            error: None,
        }
    }
}

pub type SharedSentryState = Arc<Mutex<SentryState>>;

// ── Serialisable view returned to the UI ─────────────────────────────────────

#[derive(serde::Serialize)]
struct StatusView {
    status: String,
    paused: bool,
    cpu: f32,
    memory: f32,
    disk: f32,
    failed_services: Vec<String>,
    last_analysis: String,
    recent_problems: Vec<ProblemSummary>,
    recent_executions: Vec<ExecutionSummary>,
    pending_approval: Option<ApprovalInfo>,
    error: Option<String>,
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_status(state: State<'_, SharedSentryState>) -> StatusView {
    let s = state.lock().unwrap();
    StatusView {
        status: s.status.clone(),
        paused: s.paused,
        cpu: s.cpu,
        memory: s.memory,
        disk: s.disk,
        failed_services: s.failed_services.clone(),
        last_analysis: s.last_analysis.clone(),
        recent_problems: s.recent_problems.iter().cloned().collect(),
        recent_executions: s.recent_executions.iter().cloned().collect(),
        pending_approval: s.pending_approval_info.clone(),
        error: s.error.clone(),
    }
}

#[tauri::command]
fn decide_approval(approved: bool, state: State<'_, SharedSentryState>) {
    let mut s = state.lock().unwrap();
    if let Some(tx) = s.pending_approval_tx.take() {
        let _ = tx.send(approved);
    }
    s.pending_approval_info = None;
}

#[tauri::command]
fn toggle_pause(state: State<'_, SharedSentryState>) {
    let mut s = state.lock().unwrap();
    s.paused = !s.paused;
    s.status = if s.paused {
        "Paused".to_string()
    } else {
        "Active".to_string()
    };
}

// ── Tray icon helpers ─────────────────────────────────────────────────────────

fn status_rgba(status: &str) -> Vec<u8> {
    let fill: [u8; 4] = match status {
        "Active"          => [34, 197, 94, 255],
        "Warning"         => [234, 179, 8, 255],
        "PendingApproval" => [249, 115, 22, 255],
        "Executing"       => [59, 130, 246, 255],
        "Error"           => [239, 68, 68, 255],
        _                 => [107, 114, 128, 255], // Initializing / Paused / gray
    };
    let n = 16usize;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let i = (y * n + x) * 4;
            if x == 0 || x == n - 1 || y == 0 || y == n - 1 {
                px[i]     = fill[0] / 2;
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
    let shared: SharedSentryState = Arc::new(Mutex::new(SentryState::default()));
    let shared_for_loop = shared.clone();

    tauri::Builder::default()
        .manage(shared)
        .setup(move |app| {
            let open_item =
                MenuItem::with_id(app, "open", "Open Status", true, None::<&str>)?;
            let pause_item =
                MenuItem::with_id(app, "pause", "Pause Monitoring", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit_item =
                MenuItem::with_id(app, "quit", "Quit Sentry", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;

            let tray = TrayIconBuilder::new()
                .icon(make_icon("Initializing"))
                .tooltip("Sentry — Initializing")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "pause" => {
                        let st = app.state::<SharedSentryState>();
                        let mut s = st.lock().unwrap();
                        s.paused = !s.paused;
                        s.status = if s.paused {
                            "Paused".to_string()
                        } else {
                            "Active".to_string()
                        };
                    }
                    "quit" => app.exit(0),
                    _ => {}
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

            tauri::async_runtime::spawn(async move {
                sentry_loop(shared_for_loop, tray).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            decide_approval,
            toggle_pause
        ])
        .run(tauri::generate_context!())
        .expect("Sentry failed to start");
}

// ── Decision loop ─────────────────────────────────────────────────────────────

async fn sentry_loop(state: SharedSentryState, tray: TrayIcon<tauri::Wry>) {
    macro_rules! fatal {
        ($msg:expr) => {{
            let m = $msg;
            error!("{m}");
            let mut s = state.lock().unwrap();
            s.status = "Error".to_string();
            s.error  = Some(m.clone());
            update_tray(&tray, "Error");
            return;
        }};
    }

    let cfg = match config::load("config.toml") {
        Ok(c) => c,
        Err(e) => fatal!(format!("config.toml: {e}")),
    };

    let log_level = cfg.logging.level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    let pol = match policy::ExecutionPolicy::load("policy.toml") {
        Ok(p) => p,
        Err(e) => fatal!(format!("policy.toml: {e}")),
    };

    info!(
        threshold         = pol.execution.confidence_threshold,
        rate_limit_mins   = pol.execution.rate_limit_mins,
        "Starting Sentry v0.4 (Phase 4 — Feedback Loop + Tray UI)"
    );

    let db = match audit::init_db(&cfg.persistence.audit_db).await {
        Ok(d) => d,
        Err(e) => fatal!(format!("DB init: {e}")),
    };
    let ai = match ai::client::AiClient::new(&cfg.api) {
        Ok(c) => c,
        Err(e) => fatal!(format!("AI client: {e}")),
    };

    let (event_log_shared, _el_shutdown) = signals::event_log::spawn(
        cfg.monitoring.event_log_channels.clone(),
        cfg.monitoring.event_log_poll_interval_secs,
    );
    let extra_log_dirs = cfg.monitoring.log_directories.clone();
    let initial_watch_dirs = tokio::task::spawn_blocking(move || {
        signals::file_watch::discover_watch_dirs(&extra_log_dirs)
    })
    .await
    .unwrap_or_default();
    info!(count = initial_watch_dirs.len(), "Log directories auto-discovered");
    let mut known_watch_dirs: HashSet<PathBuf> =
        initial_watch_dirs.iter().cloned().collect();
    let (file_watch_shared, _fw_shutdown, dir_update_tx) =
        signals::file_watch::spawn(initial_watch_dirs);
    let (wmi_shared, _wmi_shutdown) =
        signals::wmi::spawn(cfg.monitoring.wmi_poll_interval_secs);

    tokio::time::sleep(Duration::from_secs(5)).await;

    {
        let mut s = state.lock().unwrap();
        s.status = "Active".to_string();
        s.error  = None;
    }
    update_tray(&tray, "Active");

    let mut ticker = interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));
    info!(interval_secs = cfg.monitoring.decision_interval_secs, "Decision loop started");
    let mut cycle_count = 0u64;

    loop {
        ticker.tick().await;
        cycle_count += 1;

        // Every 20 cycles re-scan for newly active log directories
        if cycle_count.is_multiple_of(20) {
            let extra = cfg.monitoring.log_directories.clone();
            if let Ok(all) = tokio::task::spawn_blocking(move || {
                signals::file_watch::discover_watch_dirs(&extra)
            })
            .await
            {
                let mut added = 0u32;
                for dir in all {
                    if known_watch_dirs.insert(dir.clone()) {
                        let _ = dir_update_tx.send(dir);
                        added += 1;
                    }
                }
                if added > 0 {
                    info!(count = added, "Added newly discovered log directories");
                }
            }
        }

        if state.lock().unwrap().paused {
            update_tray(&tray, "Paused");
            continue;
        }

        // ── Collect signals ──────────────────────────────────────────────────
        let history = audit::get_recent_decisions(&db, 5).await.unwrap_or_else(|e| {
            warn!("Failed to load decision history: {e}");
            vec![]
        });

        let snapshot = SignalSnapshot {
            timestamp:        chrono::Utc::now(),
            event_log:        signals::event_log::snapshot(&event_log_shared),
            file_changes:     signals::file_watch::drain(&file_watch_shared),
            system_state:     signals::wmi::current(&wmi_shared),
            decision_history: history.clone(),
        };

        info!(
            event_entries = snapshot.event_log.len(),
            file_changes  = snapshot.file_changes.len(),
            "Signal snapshot collected"
        );

        // ── Update shared metrics ────────────────────────────────────────────
        {
            let st = &snapshot.system_state;
            let mut s = state.lock().unwrap();
            s.cpu             = st.cpu_usage_percent;
            s.memory          = st.memory_usage_percent;
            s.disk            = st.disk_usage_percent;
            s.failed_services = st.failed_services.clone();
        }

        // ── Feedback after-states (fill from previous cycle) ────────────────
        if let Err(e) = feedback::update_after_states(&db, &snapshot.system_state).await {
            warn!("Feedback update failed: {e}");
        }
        let feedback_summary = feedback::recent_summary(&db, 10).await.unwrap_or_default();

        // ── Claude analysis ──────────────────────────────────────────────────
        let claude_decision =
            match ai.analyze(&snapshot, &history, Some(&feedback_summary)).await {
                Ok(d) => d,
                Err(e) => {
                    error!("Claude analysis failed: {e}");
                    update_tray(&tray, "Error");
                    {
                        let mut s = state.lock().unwrap();
                        s.status = "Error".to_string();
                        s.error  = Some(format!("Claude: {e}"));
                    }
                    continue;
                }
            };

        {
            let mut s = state.lock().unwrap();
            s.last_analysis = claude_decision.analysis.clone();
            s.error         = None;
        }

        let decision_id =
            match audit::log_decision(&db, &snapshot, &claude_decision, false).await {
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

        // ── Per-problem routing ──────────────────────────────────────────────
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
                    &state,
                    &problem.diagnosis,
                    problem.confidence,
                    &problem.proposed_fix.to_string(),
                    true,
                    false,
                );
                continue;
            };

            match pol.evaluate(&action, problem.confidence) {
                // ── Blocked ──────────────────────────────────────────────────
                policy::Verdict::Block(reason) => {
                    info!(reason = %reason, "Blocked by policy");
                    push_problem(
                        &state,
                        &problem.diagnosis,
                        problem.confidence,
                        &format!("{action:?}"),
                        true,
                        false,
                    );
                }

                // ── Auto-execute ─────────────────────────────────────────────
                policy::Verdict::AutoApprove => {
                    match safety::rate_limited(&db, &action, pol.execution.rate_limit_mins).await {
                        Ok(true) => {
                            info!(action = ?action, "Rate-limited — skipping");
                            continue;
                        }
                        Err(e) => warn!("Rate limit check failed: {e}"),
                        Ok(false) => {}
                    }

                    info!(action = ?action, "AUTO-EXECUTING");
                    update_tray(&tray, "Executing");
                    {
                        let mut s = state.lock().unwrap();
                        s.status = "Executing".to_string();
                    }

                    let result = executor::execute(&action).await;

                    push_execution(&state, &result.action, result.success, &result.output);
                    push_problem(
                        &state,
                        &problem.diagnosis,
                        problem.confidence,
                        &format!("{action:?}"),
                        false,
                        true,
                    );

                    match audit::log_execution(&db, decision_id, &result).await {
                        Ok(exec_id) => {
                            if let Err(e) = feedback::record(
                                &db,
                                exec_id,
                                &result.action,
                                result.success,
                                &snapshot.system_state,
                            )
                            .await
                            {
                                error!("Failed to record feedback: {e}");
                            }
                        }
                        Err(e) => error!("Failed to log execution: {e}"),
                    }

                    {
                        let mut s = state.lock().unwrap();
                        s.status = "Active".to_string();
                    }
                    update_tray(&tray, "Active");
                }

                // ── Requires UI approval ──────────────────────────────────────
                policy::Verdict::RequireApproval(reason) => {
                    info!(reason = %reason, "Awaiting UI approval");

                    let info = ApprovalInfo {
                        diagnosis:         problem.diagnosis.clone(),
                        root_cause:        problem.root_cause.clone(),
                        confidence:        problem.confidence,
                        action:            format!("{action:?}"),
                        reason:            reason.clone(),
                        side_effects:      problem.side_effects.clone(),
                        undo_instructions: problem.undo_instructions.clone(),
                    };

                    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                    {
                        let mut s = state.lock().unwrap();
                        s.pending_approval_info = Some(info);
                        s.pending_approval_tx   = Some(tx);
                        s.status                = "PendingApproval".to_string();
                    }
                    update_tray(&tray, "PendingApproval");

                    // Wait up to 5 minutes; auto-skip on timeout
                    let approved = tokio::time::timeout(Duration::from_secs(300), rx)
                        .await
                        .unwrap_or(Ok(false))
                        .unwrap_or(false);

                    {
                        let mut s = state.lock().unwrap();
                        s.pending_approval_info = None;
                        s.pending_approval_tx   = None;
                        s.status                = "Active".to_string();
                    }
                    update_tray(&tray, "Active");

                    push_problem(
                        &state,
                        &problem.diagnosis,
                        problem.confidence,
                        &format!("{action:?}"),
                        false,
                        approved,
                    );

                    if approved {
                        info!(action = ?action, "UI-approved — executing");
                        let result = executor::execute(&action).await;
                        push_execution(&state, &result.action, result.success, &result.output);

                        match audit::log_execution(&db, decision_id, &result).await {
                            Ok(exec_id) => {
                                if let Err(e) = feedback::record(
                                    &db,
                                    exec_id,
                                    &result.action,
                                    result.success,
                                    &snapshot.system_state,
                                )
                                .await
                                {
                                    error!("Failed to record feedback: {e}");
                                }
                            }
                            Err(e) => error!("Failed to log execution: {e}"),
                        }
                    } else {
                        info!(diagnosis = %problem.diagnosis, "Rejected / timed out");
                    }
                }
            }
        }

        // Update tray based on whether there are active problems
        let tray_status = {
            let s = state.lock().unwrap();
            if s.paused {
                "Paused"
            } else if problems_found {
                "Warning"
            } else {
                "Active"
            }
            .to_string()
        };
        {
            let mut s = state.lock().unwrap();
            if !s.paused {
                s.status = tray_status.clone();
            }
        }
        update_tray(&tray, &tray_status);
    }
}

// ── State helpers ─────────────────────────────────────────────────────────────

fn push_problem(
    state: &SharedSentryState,
    diagnosis: &str,
    confidence: f32,
    action: &str,
    blocked: bool,
    auto_executed: bool,
) {
    let mut s = state.lock().unwrap();
    if s.recent_problems.len() >= 20 {
        s.recent_problems.pop_front();
    }
    s.recent_problems.push_back(ProblemSummary {
        diagnosis:     diagnosis.to_string(),
        confidence,
        action:        action.to_string(),
        blocked,
        auto_executed,
    });
}

fn push_execution(state: &SharedSentryState, action: &str, success: bool, output: &str) {
    let preview = output.chars().take(120).collect::<String>();
    let mut s = state.lock().unwrap();
    if s.recent_executions.len() >= 20 {
        s.recent_executions.pop_front();
    }
    s.recent_executions.push_back(ExecutionSummary {
        action:  action.to_string(),
        success,
        preview,
    });
}
