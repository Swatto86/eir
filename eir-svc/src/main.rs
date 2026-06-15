mod ai;
mod audit;
mod config;
mod executor;
mod feedback;
mod models;
mod pipe_server;
mod policy;
mod safety;
mod signals;

use models::SignalSnapshot;
use eir_proto::{
    ApprovalInfo, ExecutionSummary, ProblemSummary, StatusPayload, UiMsg, UiSettings, UsageSummary,
};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
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

fn run_service() -> windows_service::Result<()> {
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_signal = shutdown.clone();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                shutdown_signal.store(true, std::sync::atomic::Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
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

    rt.block_on(eir_main(async move {
        // Poll the atomic flag until Stop/Shutdown is received
        loop {
            if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }));

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

fn install_service() {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)
        .expect("Failed to connect to service manager (run as Administrator)");
    let exe_path = std::env::current_exe().expect("Cannot get executable path");
    let svc_info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = manager
        .create_service(&svc_info, ServiceAccess::ALL_ACCESS)
        .expect("Failed to create service");
    svc.set_description("Autonomous Windows system repair agent powered by AI")
        .expect("Failed to set description");
    println!("{SERVICE_NAME} installed successfully.");
    println!("Start it with:  sc start {SERVICE_NAME}");
    println!("Stop it with:   sc stop {SERVICE_NAME}");
}

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("install") => install_service(),
        Some("uninstall") => uninstall_service(),
        _ => {
            // Try SCM dispatch; on failure run standalone (development / debugging).
            if service_dispatcher::start(SERVICE_NAME, ffi_service_main).is_err() {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Tokio runtime");
                // Standalone: run until Ctrl-C
                rt.block_on(eir_main(async {
                    let _ = tokio::signal::ctrl_c().await;
                }));
            }
        }
    }
}

// ── Service state ─────────────────────────────────────────────────────────────

struct SvcState {
    paused: bool,
    cpu: f32,
    memory: f32,
    disk: f32,
    failed_services: Vec<String>,
    last_analysis: String,
    recent_problems: VecDeque<ProblemSummary>,
    recent_executions: VecDeque<ExecutionSummary>,
    status: String,
    error: Option<String>,
    usage: Option<UsageSummary>,
    settings: Option<UiSettings>,
}

impl Default for SvcState {
    fn default() -> Self {
        Self {
            paused: false,
            cpu: 0.0,
            memory: 0.0,
            disk: 0.0,
            failed_services: vec![],
            last_analysis: String::new(),
            recent_problems: VecDeque::new(),
            recent_executions: VecDeque::new(),
            status: "Initializing".to_string(),
            error: None,
            usage: None,
            settings: None,
        }
    }
}

/// Restart the service to apply new settings: a detached helper stops then
/// starts EirSvc (LocalSystem — no UAC). It survives this process exiting.
fn restart_self() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let _ = std::process::Command::new("cmd")
        .args([
            "/C",
            "sc stop EirSvc & ping -n 4 127.0.0.1 >nul & sc start EirSvc",
        ])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();
}

fn build_status(st: &SvcState, pending: Option<ApprovalInfo>) -> StatusPayload {
    StatusPayload {
        status: st.status.clone(),
        paused: st.paused,
        cpu: st.cpu,
        memory: st.memory,
        disk: st.disk,
        failed_services: st.failed_services.clone(),
        last_analysis: st.last_analysis.clone(),
        recent_problems: st.recent_problems.iter().cloned().collect(),
        recent_executions: st.recent_executions.iter().cloned().collect(),
        pending_approval: pending,
        error: st.error.clone(),
        usage: st.usage.clone(),
        settings: st.settings.clone(),
    }
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
            if le.severity != "INFO" || !le.error_snippets.is_empty() {
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
        if e.level == "Error" || e.level == "Warning" {
            parts.push(format!("E|{}|{}|{}", e.level, e.source, e.event_id));
        }
    }
    let sys = &snap.system_state;
    for s in &sys.failed_services {
        parts.push(format!("S|{s}"));
    }
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

fn push_execution(st: &mut SvcState, action: &str, success: bool, output: &str) {
    let preview = output.chars().take(120).collect::<String>();
    if st.recent_executions.len() >= 20 {
        st.recent_executions.pop_front();
    }
    st.recent_executions.push_back(ExecutionSummary {
        action: action.to_string(),
        success,
        preview,
        at: chrono::Utc::now().timestamp(),
    });
}

// ── Decision loop ─────────────────────────────────────────────────────────────

async fn eir_main<F: std::future::Future<Output = ()>>(shutdown: F) {
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

    let (pipe, mut ui_rx) = pipe_server::spawn();
    let mut st = SvcState::default();

    macro_rules! fatal {
        ($msg:expr) => {{
            let m = $msg;
            error!("{m}");
            st.status = "Error".to_string();
            st.error = Some(m.clone());
            pipe.broadcast_status(build_status(&st, None));
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
        threshold = pol.execution.confidence_threshold,
        rate_limit_mins = pol.execution.rate_limit_mins,
        "Starting Eir v0.6 — service mode"
    );

    let db_path = config::resolve(&cfg.persistence.audit_db);
    let db = match audit::init_db(db_path.to_str().unwrap_or(&cfg.persistence.audit_db)).await {
        Ok(d) => d,
        Err(e) => fatal!(format!("DB init: {e}")),
    };
    // A bad AI config must NOT kill the service — degrade instead, so the pipe
    // and UI stay alive and the user can fix it in Settings.
    let ai = match ai::client::AiClient::new(&cfg.api) {
        Ok(c) => Some(c),
        Err(e) => {
            error!("AI client init failed: {e}");
            st.status = "Error".to_string();
            st.error = Some(format!(
                "AI provider not configured: {e} — fix it in Settings"
            ));
            None
        }
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
    info!(
        count = initial_watch_dirs.len(),
        "Log directories auto-discovered"
    );
    let mut known_watch_dirs: HashSet<PathBuf> = initial_watch_dirs.iter().cloned().collect();
    let (file_watch_shared, _fw_shutdown, dir_update_tx) =
        signals::file_watch::spawn(initial_watch_dirs);
    let (wmi_shared, _wmi_shutdown) = signals::wmi::spawn(cfg.monitoring.wmi_poll_interval_secs);

    tokio::time::sleep(Duration::from_secs(5)).await;

    st.status = "Active".to_string();
    st.error = None;
    if let Ok(s) = audit::usage_summary(&db).await {
        st.usage = Some(s);
    }
    pipe.broadcast_status(build_status(&st, None));

    let mut ticker = interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));
    info!(
        interval_secs = cfg.monitoring.decision_interval_secs,
        "Decision loop started"
    );
    let mut cycle_count = 0u64;
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
                tokio::select! {
                    _ = ticker.tick() => {}
                    Some(cmd) = ui_rx.recv() => {
                        match cmd {
                        UiMsg::TogglePause => {
                            st.paused = !st.paused;
                            st.status = if st.paused {
                                "Paused".to_string()
                            } else {
                                "Active".to_string()
                            };
                            pipe.broadcast_status(build_status(&st, None));
                        }
                        UiMsg::ClearProblems => {
                            st.recent_problems.clear();
                            pipe.broadcast_status(build_status(&st, None));
                        }
                        UiMsg::ClearExecutions => {
                            st.recent_executions.clear();
                            pipe.broadcast_status(build_status(&st, None));
                        }
                        UiMsg::UpdateSettings(update) => {
                            cfg.apply_update(*update);
                            // Validate before committing — never restart into a broken
                            // provider (e.g. openrouter with no key would brick the service).
                            if let Err(e) = ai::client::AiClient::new(&cfg.api) {
                                warn!("Rejected settings: {e}");
                                st.error = Some(format!("Settings not applied — {e}"));
                                if let Ok(reloaded) = config::load("config.toml") {
                                    cfg = reloaded; // discard the invalid change
                                }
                                st.settings = Some(cfg.to_ui_settings());
                                pipe.broadcast_status(build_status(&st, None));
                            } else {
                                st.settings = Some(cfg.to_ui_settings());
                                match config::save(&cfg, "config.toml") {
                                    Ok(()) => {
                                        info!("Settings saved — restarting service to apply");
                                        st.status = "Restarting".to_string();
                                        st.error = None;
                                        pipe.broadcast_status(build_status(&st, None));
                                        restart_self();
                                        return; // SCM stop will follow; exit cleanly now
                                    }
                                    Err(e) => {
                                        error!("Failed to save settings: {e}");
                                        st.error = Some(format!("Save settings: {e}"));
                                        pipe.broadcast_status(build_status(&st, None));
                                    }
                                }
                            }
                        }
                        UiMsg::Approve { .. } => {} // resolved in pipe_server
                        }
                        continue;
                    }
                }
                cycle_count += 1;

                // Re-discover log directories every 20 cycles
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

                // ── Update metrics in broadcast ──────────────────────────────
                {
                    let sys = &snapshot.system_state;
                    st.cpu             = sys.cpu_usage_percent;
                    st.memory          = sys.memory_usage_percent;
                    st.disk            = sys.disk_usage_percent;
                    st.failed_services = sys.failed_services.clone();
                }
                pipe.broadcast_status(build_status(&st, None));

                // No AI provider configured — keep collecting signals and serving
                // the UI (so Settings stays usable), but skip analysis.
                let Some(ai) = ai.as_ref() else {
                    continue;
                };

                // ── Feedback after-states ────────────────────────────────────
                if let Err(e) =
                    feedback::update_after_states(&db, &snapshot.system_state).await
                {
                    warn!("Feedback update failed: {e}");
                }
                let feedback_summary =
                    feedback::recent_summary(&db, 10).await.unwrap_or_default();

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
                    info!("No actionable change since last analysis — skipping");
                    if !st.paused {
                        st.status = "Active".to_string();
                        st.error = None;
                    }
                    pipe.broadcast_status(build_status(&st, None));
                    continue;
                }

                // ── Claude analysis ──────────────────────────────────────────
                let claude_decision =
                    match ai.analyze(&snapshot, &history, Some(&feedback_summary)).await {
                        Ok((d, usage)) => {
                            if let Some(u) = usage {
                                if let Err(e) = audit::log_usage(&db, &u).await {
                                    warn!("Failed to log usage: {e}");
                                }
                                match audit::usage_summary(&db).await {
                                    Ok(s) => st.usage = Some(s),
                                    Err(e) => warn!("Failed to compute usage summary: {e}"),
                                }
                            }
                            d
                        }
                        Err(e) => {
                            error!("AI analysis failed: {e}");
                            st.status = "Error".to_string();
                            st.error  = Some(format!("AI: {e}"));
                            pipe.broadcast_status(build_status(&st, None));
                            continue;
                        }
                    };

                // Remember this state + time so unchanged idle cycles are skipped
                // until the next heartbeat.
                last_fingerprint = fingerprint;
                last_analysis_at = Some(std::time::Instant::now());

                st.last_analysis = claude_decision.analysis.clone();
                st.error         = None;

                let decision_id = match audit::log_decision(
                    &db,
                    &snapshot,
                    &claude_decision,
                )
                .await
                {
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
                        pipe.broadcast_status(build_status(&st, None));
                        continue;
                    };

                    match pol.evaluate(&action, problem.confidence) {
                        policy::Verdict::Block(reason) => {
                            info!(reason = %reason, "Blocked by policy");
                            push_problem(
                                &mut st,
                                &problem.diagnosis,
                                problem.confidence,
                                &format!("{action:?}"),
                                true,
                                false,
                                Some(reason),
                            );
                            pipe.broadcast_status(build_status(&st, None));
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
                                Err(e) => warn!("Rate limit check failed: {e}"),
                                Ok(false) => {}
                            }

                            info!(action = ?action, "AUTO-EXECUTING");
                            st.status = "Executing".to_string();
                            pipe.broadcast_status(build_status(&st, None));

                            let result = executor::execute(&action).await;

                            push_execution(&mut st, &result.action, result.success, &result.output);
                            push_problem(
                                &mut st,
                                &problem.diagnosis,
                                problem.confidence,
                                &format!("{action:?}"),
                                false,
                                true,
                                None,
                            );
                            st.status = "Active".to_string();
                            pipe.broadcast_status(build_status(&st, None));

                            match audit::log_execution(&db, decision_id, &result).await {
                                Ok(exec_id) => {
                                    if let Err(e) =
                                        audit::mark_decision_executed(&db, decision_id).await
                                    {
                                        error!("Failed to mark decision executed: {e}");
                                    }
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
                        }

                        policy::Verdict::RequireApproval(reason) => {
                            info!(reason = %reason, "Awaiting UI approval");

                            let id = pipe.next_approval_id();
                            let info = ApprovalInfo {
                                id,
                                diagnosis:         problem.diagnosis.clone(),
                                root_cause:        problem.root_cause.clone(),
                                confidence:        problem.confidence,
                                action:            format!("{action:?}"),
                                reason:            reason.clone(),
                                side_effects:      problem.side_effects.clone(),
                                undo_instructions: problem.undo_instructions.clone(),
                            };

                            st.status = "PendingApproval".to_string();
                            let approval_status = build_status(&st, Some(info.clone()));

                            let approved =
                                pipe.request_approval(id, approval_status).await;

                            st.status = "Active".to_string();
                            push_problem(
                                &mut st,
                                &problem.diagnosis,
                                problem.confidence,
                                &format!("{action:?}"),
                                false,
                                approved,
                                Some(reason),
                            );
                            pipe.broadcast_status(build_status(&st, None));

                            if approved {
                                info!(action = ?action, "UI-approved — executing");
                                let result = executor::execute(&action).await;
                                push_execution(
                                    &mut st,
                                    &result.action,
                                    result.success,
                                    &result.output,
                                );
                                pipe.broadcast_status(build_status(&st, None));

                                match audit::log_execution(&db, decision_id, &result).await {
                                    Ok(exec_id) => {
                                        if let Err(e) =
                                            audit::mark_decision_executed(&db, decision_id).await
                                        {
                                            error!("Failed to mark decision executed: {e}");
                                        }
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

                let tray_status = if st.paused {
                    "Paused"
                } else if problems_found {
                    "Warning"
                } else {
                    "Active"
                }
                .to_string();
                if !st.paused {
                    st.status = tray_status;
                }
                pipe.broadcast_status(build_status(&st, None));
            }
        } => {}
        _ = shutdown => {
            info!("Shutdown signal received — stopping service loop");
        }
    }
}
