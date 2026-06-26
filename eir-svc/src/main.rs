mod ai;
mod audit;
mod config;
mod executor;
mod explain;
mod feedback;
mod learn;
mod models;
mod pipe_server;
mod policy;
mod safety;
mod signals;
mod updater;

use eir_proto::{
    AdvisorStatus, ApprovalInfo, ExecutionSummary, LearnedFactView, ProblemSummary, StatusPayload,
    UiMsg, UiSettings, UpdaterStatus, UsageSummary,
};
use models::{ExecutionResult, FixAction, PendingApproval, SignalSnapshot, SystemState};
use sqlx::SqlitePool;
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
    /// Escalations performed today — a provider-agnostic backstop, since the USD
    /// budget can't bound providers that report no cost.
    advisor_escalations_today: u32,
    /// The UTC date (YYYY-MM-DD) that the advisor day-counters belong to.
    advisor_spend_date: String,
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

fn build_status(st: &SvcState) -> StatusPayload {
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
        pending_approvals: st.pending.iter().map(|p| p.info.clone()).collect(),
        error: st.error.clone(),
        usage: st.usage.clone(),
        settings: st.settings.clone(),
        updater: Some(st.updater.clone()),
        advisor: st.advisor.clone(),
        learned_facts: st.learned_facts.clone(),
    }
}

/// Hard backstop on escalations per UTC day. The USD budget can only bound providers
/// that report cost (OpenRouter, and the Claude CLI when it does); for the Anthropic
/// native and OpenAI-compatible providers — which return no usage — this count is the
/// only ceiling. Always applied so the cap holds regardless of provider.
const MAX_ESCALATIONS_PER_DAY: u32 = 24;

/// Decide whether the advisor should re-analyse at a higher tier, and why. Pure.
/// Returns `Some(reason)` to escalate. Bounded: it fires only when advisor mode is on,
/// a deeper tier is configured, neither the per-day count cap nor the USD budget is
/// spent, AND the agent flagged ambiguity or the best confidence is below the threshold.
fn should_escalate(
    decision: &models::ClaudeDecision,
    cfg: &config::AdvisorConfig,
    spent_today: f64,
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
    if cfg.budget_usd_per_day > 0.0 && spent_today >= cfg.budget_usd_per_day {
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
) {
    let ai = ai::client::AiClient::new(&cfg.api).ok();
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
        // Run the cycle in an inner task so a panic surfaces as a JoinError (not a
        // silent abort) AND a watchdog can stop a hang — either way a summary is sent
        // and `updater_running` is released, so the updater can never latch "running"
        // forever and wedge every future cycle.
        let mut inner = tokio::spawn(async move {
            let ctx = updater::orchestrator::EngineCtx {
                ai: ai.as_ref(),
                config: &updater_cfg,
                model_override: &model,
            };
            updater::orchestrator::run_cycle(&pool, &ctx, cycle_id, &progress).await
        });
        let summary = match tokio::time::timeout(CYCLE_MAX, &mut inner).await {
            Ok(Ok(summary)) => summary,
            Ok(Err(_join)) => updater::orchestrator::CycleSummary {
                results: Vec::new(),
                notes: vec!["update cycle aborted unexpectedly".to_string()],
                cost_usd: 0.0,
            },
            Err(_elapsed) => {
                inner.abort();
                updater::orchestrator::CycleSummary {
                    results: Vec::new(),
                    notes: vec![format!(
                        "update cycle exceeded {}m and was stopped",
                        CYCLE_MAX.as_secs() / 60
                    )],
                    cost_usd: 0.0,
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
    /// `format!("{action:?}")` — the dedupe key and the label shown in the activity feed.
    label: String,
    diagnosis: String,
    confidence: f32,
    /// Why it ran (e.g. "approved by user"); None for an autonomous fix.
    reason: Option<String>,
}

/// The result of one executor job, folded back into `SvcState` on the decision loop.
struct ExecOutcome {
    label: String,
    /// `result.action` (the executed action's Debug form) for the execution log entry.
    exec_action: String,
    success: bool,
    output: String,
    diagnosis: String,
    confidence: f32,
    reason: Option<String>,
}

/// Spawn the single executor worker. It serialises fix-action execution off the
/// decision loop: each job runs `executor::execute` (panic-isolated) and writes the
/// audit/feedback records, then reports an [`ExecOutcome`] back over `done_tx` for the
/// loop to fold into `SvcState`. Because execution no longer blocks the loop, UI
/// commands and status updates stay responsive however long an action takes.
fn spawn_executor(
    db: &SqlitePool,
    mut job_rx: tokio::sync::mpsc::UnboundedReceiver<ExecJob>,
    done_tx: tokio::sync::mpsc::UnboundedSender<ExecOutcome>,
) {
    let db = db.clone();
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let action = job.action;
            // Isolate a panicking action so the worker survives to run the next job.
            let result = match tokio::spawn(async move { executor::execute(&action).await }).await {
                Ok(r) => r,
                Err(_join) => ExecutionResult {
                    action: job.label.clone(),
                    success: false,
                    output: "execution task panicked".to_string(),
                },
            };

            match audit::log_execution(&db, job.decision_id, &result).await {
                Ok(exec_id) => {
                    if let Err(e) = audit::mark_decision_executed(&db, job.decision_id).await {
                        error!("Failed to mark decision executed: {e}");
                    }
                    if let Err(e) = feedback::record(
                        &db,
                        exec_id,
                        &result.action,
                        result.success,
                        &job.baseline,
                    )
                    .await
                    {
                        error!("Failed to record feedback: {e}");
                    }
                }
                Err(e) => error!("Failed to log execution: {e}"),
            }

            // If the loop has gone away the whole service is shutting down — ignore.
            let _ = done_tx.send(ExecOutcome {
                label: job.label,
                exec_action: result.action,
                success: result.success,
                output: result.output,
                diagnosis: job.diagnosis,
                confidence: job.confidence,
                reason: job.reason,
            });
        }
    });
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
    // Security posture — a concrete, fixable exposure makes the cycle actionable.
    // Only Some(false)/stale counts; unknown (None) and healthy (true) do not, so a
    // secure machine stays idle and doesn't burn AI calls.
    let sec = &sys.security;
    for (name, on) in [
        ("domain", sec.firewall.domain),
        ("private", sec.firewall.private),
        ("public", sec.firewall.public),
    ] {
        if on == Some(false) {
            parts.push(format!("FW|{name}"));
        }
    }
    // Defender faults only matter when Defender is the active AV — if a third-party
    // AV has taken over (antivirus_enabled == false) its passive state is normal.
    if sec.defender.antivirus_enabled != Some(false) {
        if sec.defender.realtime_enabled == Some(false) {
            parts.push("DEF|realtime_off".into());
        }
        if sec.defender.signature_age_days.is_some_and(|d| d > 3) {
            parts.push("DEF|sig_stale".into());
        }
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
    // Seed the updater status from config + history, and clear any stale install
    // staging left by a previous run.
    updater::download::cleanup_stale_staging();
    st.updater = UpdaterStatus {
        enabled: cfg.updater.enabled,
        settings: cfg.updater.to_view(),
        ..Default::default()
    };
    if let Ok(recent) = updater::history::recent(&db, 50).await {
        st.updater.recent = recent;
    }
    st.advisor = Some(AdvisorStatus {
        enabled: cfg.advisor.enabled,
        settings: cfg.advisor.to_view(),
        ..Default::default()
    });
    st.advisor_spend_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
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
    st.status = resting_status(&st);
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
    spawn_executor(&db, exec_rx, exec_done_tx);
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
                    Some(summary) = update_done_rx.recv() => {
                        // An update cycle finished — fold its result into the status.
                        st.updater_running = false;
                        st.updater.running = false;
                        st.updater.last_run = chrono::Utc::now().timestamp();
                        st.updater.last_cost_usd = summary.cost_usd;
                        st.updater.notes = summary.notes.clone();
                        st.updater.apps = updater::orchestrator::app_rows(&summary);
                        st.updater.phase = "idle".to_string();
                        if let Ok(recent) = updater::history::recent(&db, 50).await {
                            st.updater.recent = recent;
                        }
                        st.updater.next_run = if cfg.updater.enabled {
                            st.updater.last_run + cfg.updater.schedule_interval_secs as i64
                        } else {
                            0
                        };
                        info!(apps = st.updater.apps.len(), "Update cycle complete");
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
                        st.in_flight.remove(&outcome.label);
                        push_execution(&mut st, &outcome.exec_action, outcome.success, &outcome.output);
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
                    Some(cmd) = ui_rx.recv() => {
                        match cmd {
                        UiMsg::TogglePause => {
                            st.paused = !st.paused;
                            st.status = resting_status(&st);
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::ClearProblems => {
                            st.recent_problems.clear();
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::ClearExecutions => {
                            st.recent_executions.clear();
                            pipe.broadcast_status(build_status(&st));
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
                                pipe.broadcast_status(build_status(&st));
                            } else {
                                st.settings = Some(cfg.to_ui_settings());
                                match config::save(&cfg, "config.toml") {
                                    Ok(()) => {
                                        info!("Settings saved — restarting service to apply");
                                        st.status = "Restarting".to_string();
                                        st.error = None;
                                        pipe.broadcast_status(build_status(&st));
                                        restart_self();
                                        return; // SCM stop will follow; exit cleanly now
                                    }
                                    Err(e) => {
                                        error!("Failed to save settings: {e}");
                                        st.error = Some(format!("Save settings: {e}"));
                                        pipe.broadcast_status(build_status(&st));
                                    }
                                }
                            }
                        }
                        UiMsg::Approve { id, approved } => {
                            // Resolve a queued approval. Find it, remove it from
                            // both memory and the DB, then act on the decision.
                            if let Some(pos) =
                                st.pending.iter().position(|p| p.info.id == id)
                            {
                                let pa = st.pending.remove(pos);
                                if let Err(e) = audit::delete_pending_approval(&db, id).await {
                                    warn!("Failed to delete pending approval {id}: {e}");
                                }
                                if approved {
                                    let label = pa.info.action.clone();
                                    if st.in_flight.contains(&label) {
                                        // Same action already running off-loop — resolve
                                        // this card without enqueueing a second run.
                                        info!(action = %label, "Approved action already executing — not re-running");
                                    } else {
                                        info!(action = ?pa.action, "UI-approved — queueing for execution");
                                        // Hand off to the executor worker; the outcome
                                        // (execution log + problem entry) is folded in
                                        // when it finishes, so the loop stays responsive.
                                        st.in_flight.insert(label.clone());
                                        let _ = exec_tx.send(ExecJob {
                                            action: pa.action,
                                            decision_id: pa.decision_id,
                                            baseline: pa.baseline,
                                            label,
                                            diagnosis: pa.info.diagnosis.clone(),
                                            confidence: pa.info.confidence,
                                            reason: Some("approved by user".into()),
                                        });
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
                                }
                                st.error = None;
                            }
                            // Whether resolved or stale, settle the status and
                            // refresh the UI (the card disappears from the queue).
                            st.status = resting_status(&st);
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::RunUpdatesNow => {
                            // Gate the manual trigger on the SAME controls as the
                            // scheduled run: the master switch and pause. The command
                            // pipe is writable by any authenticated user (so the
                            // Medium-integrity UI can send it), so a manual run must
                            // not be able to override the admin's enabled/pause state.
                            if cfg.updater.enabled && !st.paused && !st.updater_running {
                                st.updater_running = true;
                                st.updater.running = true;
                                st.updater.enabled = true;
                                st.updater.phase = "checking…".to_string();
                                pipe.broadcast_status(build_status(&st));
                                spawn_update_cycle(&cfg, &db, &update_done_tx, &update_progress_tx);
                            }
                        }
                        UiMsg::ClearUpdateHistory => {
                            // Wipe the persisted attempt log and the last cycle's
                            // in-memory results so the card resets to a clean state.
                            if let Err(e) = updater::history::clear(&db).await {
                                warn!("Failed to clear update history: {e}");
                            }
                            // Also reset detector-learned facts: they are derived purely
                            // from the attempt log just cleared, so leaving them would keep
                            // skipping apps with no remaining evidence. User pinned/disabled
                            // facts are preserved by clear_detector_facts.
                            if let Err(e) = learn::clear_detector_facts(&db).await {
                                warn!("Failed to clear learned facts: {e}");
                            }
                            st.updater.recent.clear();
                            st.updater.apps.clear();
                            st.updater.notes.clear();
                            st.learned_facts = learn::facts_for_view(&db).await.unwrap_or_default();
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::SetLearnedFact { id, op } => {
                            if let Err(e) = learn::set_learned_fact(&db, id, &op).await {
                                warn!("Failed to set learned fact {id} ({op}): {e}");
                            }
                            st.learned_facts = learn::facts_for_view(&db).await.unwrap_or_default();
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::UpdateUpdaterSettings(update) => {
                            // Applied live — no service restart (unlike provider settings).
                            cfg.updater.apply_view(*update);
                            st.updater.enabled = cfg.updater.enabled;
                            st.updater.settings = cfg.updater.to_view();
                            st.updater.next_run = if !cfg.updater.enabled {
                                0
                            } else if st.updater.last_run > 0 {
                                st.updater.last_run + cfg.updater.schedule_interval_secs as i64
                            } else {
                                chrono::Utc::now().timestamp()
                                    + cfg.updater.schedule_interval_secs as i64
                            };
                            if let Err(e) = config::save(&cfg, "config.toml") {
                                warn!("Failed to save updater settings: {e}");
                            }
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::SetAppIgnore { id, ignore, note } => {
                            let key = id.to_lowercase();
                            if ignore {
                                if !cfg.updater.ignored.iter().any(|x| x.eq_ignore_ascii_case(&key))
                                {
                                    cfg.updater.ignored.push(key.clone());
                                }
                            } else {
                                cfg.updater.ignored.retain(|x| !x.eq_ignore_ascii_case(&key));
                            }
                            let n = note.trim();
                            if n.is_empty() {
                                cfg.updater.notes.remove(&key);
                            } else {
                                cfg.updater.notes.insert(key, n.to_string());
                            }
                            if let Err(e) = config::save(&cfg, "config.toml") {
                                warn!("Failed to save app note: {e}");
                            }
                            pipe.broadcast_status(build_status(&st));
                        }
                        UiMsg::SetAdvisorSettings(update) => {
                            // Applied live — no service restart.
                            cfg.advisor.apply_view(*update);
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
                            if let Err(e) = config::save(&cfg, "config.toml") {
                                warn!("Failed to save advisor settings: {e}");
                            }
                            pipe.broadcast_status(build_status(&st));
                        }
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

                // ── Autonomous updater: start a scheduled cycle when due ──────
                {
                    let now = chrono::Utc::now().timestamp();
                    let interval_secs = cfg.updater.schedule_interval_secs as i64;
                    let due = cfg.updater.enabled
                        && !st.paused
                        && !st.updater_running
                        && (st.updater.last_run == 0 || now - st.updater.last_run >= interval_secs);
                    if due {
                        info!("Autonomous update cycle due — starting");
                        st.updater_running = true;
                        st.updater.running = true;
                        st.updater.enabled = true;
                        st.updater.phase = "checking…".to_string();
                        pipe.broadcast_status(build_status(&st));
                        spawn_update_cycle(&cfg, &db, &update_done_tx, &update_progress_tx);
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
                pipe.broadcast_status(build_status(&st));

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

                // Tier-2 (optional): give one not-yet-explained learned fact a one-sentence
                // human-readable AI explanation (text only — never changes behaviour). Skips
                // the AI call entirely when every fact is already labelled.
                learn::label_one(&db, ai, &cfg.api.update_check_model).await;

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
                let learned_section = learned_facts.prompt_section();

                // ── Claude analysis ──────────────────────────────────────────
                let mut claude_decision =
                    match ai
                        .analyze(&snapshot, &history, Some(&feedback_summary), learned_section.as_deref())
                        .await
                    {
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
                            pipe.broadcast_status(build_status(&st));
                            continue;
                        }
                    };

                // Remember this state + time so unchanged idle cycles are skipped
                // until the next heartbeat.
                last_fingerprint = fingerprint;
                last_analysis_at = Some(std::time::Instant::now());

                st.last_analysis = claude_decision.analysis.clone();
                st.error         = None;

                // ── Advisor mode: re-think harder once if warranted ───────────
                {
                    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                    if st.advisor_spend_date != today {
                        st.advisor_spend_date = today;
                        st.advisor_spent_today = 0.0;
                        st.advisor_escalations_today = 0;
                    }
                    let mut adv = AdvisorStatus {
                        enabled: cfg.advisor.enabled,
                        escalated: false,
                        escalation_model: String::new(),
                        reason: String::new(),
                        spent_today_usd: st.advisor_spent_today,
                        settings: cfg.advisor.to_view(),
                    };
                    if let Some(reason) = should_escalate(
                        &claude_decision,
                        &cfg.advisor,
                        st.advisor_spent_today,
                        st.advisor_escalations_today,
                    ) {
                        info!(reason, "Advisor escalating to a deeper analysis pass");
                        // Count the attempt (even if it fails) so a failing escalation
                        // can't retry every cycle and defeat the cap.
                        st.advisor_escalations_today += 1;
                        match ai
                            .analyze_with(
                                &snapshot,
                                &history,
                                Some(&feedback_summary),
                                learned_section.as_deref(),
                                Some(cfg.advisor.escalation_model.as_str()),
                                Some(cfg.advisor.escalation_effort.as_str()),
                            )
                            .await
                        {
                            Ok((d2, usage2)) => {
                                if let Some(u) = usage2 {
                                    st.advisor_spent_today += u.cost_usd;
                                    if let Err(e) = audit::log_usage(&db, &u).await {
                                        warn!("Failed to log escalation usage: {e}");
                                    }
                                    if let Ok(s) = audit::usage_summary(&db).await {
                                        st.usage = Some(s);
                                    }
                                }
                                claude_decision = d2;
                                st.last_analysis = claude_decision.analysis.clone();
                                adv.escalated = true;
                                adv.reason = reason.to_string();
                                adv.escalation_model = cfg.advisor.escalation_model.clone();
                                adv.spent_today_usd = st.advisor_spent_today;
                            }
                            Err(e) => warn!("Advisor escalation failed: {e}"),
                        }
                    }
                    st.advisor = Some(adv);
                }

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
                            push_problem(
                                &mut st,
                                &problem.diagnosis,
                                problem.confidence,
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
                                Err(e) => warn!("Rate limit check failed: {e}"),
                                Ok(false) => {}
                            }

                            let label = format!("{action:?}");
                            if st.in_flight.contains(&label) {
                                info!(action = %label, "Already executing — skipping duplicate");
                                continue;
                            }
                            info!(action = ?action, "AUTO-EXECUTING (queued)");
                            // Hand off to the executor worker and move on; the outcome
                            // is folded in when it finishes (see exec_done_rx arm).
                            st.in_flight.insert(label.clone());
                            let _ = exec_tx.send(ExecJob {
                                action: action.clone(),
                                decision_id,
                                baseline: snapshot.system_state.clone(),
                                label,
                                diagnosis: problem.diagnosis.clone(),
                                confidence: problem.confidence,
                                reason: None,
                            });
                            st.status = "Executing".to_string();
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
                            if st.pending.iter().any(|p| p.info.action == action_label)
                                || st.in_flight.contains(&action_label)
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
                                target_details:    explain::target_details(&action),
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
                                    info.id = row_id as u64;
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
            }
        } => {}
        _ = shutdown => {
            info!("Shutdown signal received — stopping service loop");
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    /// resting_status must order Paused > PendingApproval > Executing > Active, so an
    /// off-loop fix keeps the UI on "Executing" (the off-loop-execution invariant).
    #[test]
    fn resting_status_precedence() {
        let mut st = SvcState::default();
        assert_eq!(resting_status(&st), "Active");

        st.in_flight.insert("ServiceRestart".into());
        assert_eq!(resting_status(&st), "Executing");

        // Paused outranks an in-flight execution.
        st.paused = true;
        assert_eq!(resting_status(&st), "Paused");
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
            budget_usd_per_day: 0.50,
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
    fn budget_count_and_missing_tier_block_escalation() {
        // Over the daily USD budget -> don't, even with the AI flag.
        assert!(should_escalate(&decision(true, &[]), &cfg(true), 0.50, 0).is_none());
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
