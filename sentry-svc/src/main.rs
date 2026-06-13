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
use sentry_proto::{ApprovalInfo, ExecutionSummary, ProblemSummary, StatusPayload, UiMsg};
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

const SERVICE_NAME: &str = "SentrySvc";
const SERVICE_DISPLAY: &str = "Sentry System Monitor";

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

    rt.block_on(sentry_main(async move {
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
    svc.set_description("Autonomous Windows system repair agent powered by Claude AI")
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
                rt.block_on(sentry_main(async {
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
        }
    }
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
    }
}

fn push_problem(
    st: &mut SvcState,
    diagnosis: &str,
    confidence: f32,
    action: &str,
    blocked: bool,
    auto_executed: bool,
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
    });
}

// ── Decision loop ─────────────────────────────────────────────────────────────

async fn sentry_main<F: std::future::Future<Output = ()>>(shutdown: F) {
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

    let cfg = match config::load("config.toml") {
        Ok(c) => c,
        Err(e) => fatal!(format!("config.toml: {e}")),
    };

    let log_level = cfg.logging.level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    let pol = match policy::ExecutionPolicy::load(
        config::resolve("policy.toml").to_str().unwrap_or("policy.toml"),
    ) {
        Ok(p) => p,
        Err(e) => fatal!(format!("policy.toml: {e}")),
    };

    info!(
        threshold = pol.execution.confidence_threshold,
        rate_limit_mins = pol.execution.rate_limit_mins,
        "Starting Sentry v0.5 — service mode"
    );

    let db_path = config::resolve(&cfg.persistence.audit_db);
    let db = match audit::init_db(db_path.to_str().unwrap_or(&cfg.persistence.audit_db)).await {
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
    pipe.broadcast_status(build_status(&st, None));

    let mut ticker = interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));
    info!(
        interval_secs = cfg.monitoring.decision_interval_secs,
        "Decision loop started"
    );
    let mut cycle_count = 0u64;

    let shutdown = std::pin::pin!(shutdown);
    tokio::select! {
        _ = async {
            loop {
                ticker.tick().await;
                cycle_count += 1;

                // Handle pending UI commands
                while let Ok(cmd) = ui_rx.try_recv() {
                    if let UiMsg::TogglePause = cmd {
                        st.paused = !st.paused;
                        st.status = if st.paused {
                            "Paused".to_string()
                        } else {
                            "Active".to_string()
                        };
                        pipe.broadcast_status(build_status(&st, None));
                    }
                }

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

                // ── Feedback after-states ────────────────────────────────────
                if let Err(e) =
                    feedback::update_after_states(&db, &snapshot.system_state).await
                {
                    warn!("Feedback update failed: {e}");
                }
                let feedback_summary =
                    feedback::recent_summary(&db, 10).await.unwrap_or_default();

                // ── Claude analysis ──────────────────────────────────────────
                let claude_decision =
                    match ai.analyze(&snapshot, &history, Some(&feedback_summary)).await {
                        Ok(d) => d,
                        Err(e) => {
                            error!("Claude analysis failed: {e}");
                            st.status = "Error".to_string();
                            st.error  = Some(format!("Claude: {e}"));
                            pipe.broadcast_status(build_status(&st, None));
                            continue;
                        }
                    };

                st.last_analysis = claude_decision.analysis.clone();
                st.error         = None;

                let decision_id = match audit::log_decision(
                    &db,
                    &snapshot,
                    &claude_decision,
                    false,
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
                            );
                            st.status = "Active".to_string();
                            pipe.broadcast_status(build_status(&st, None));

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
