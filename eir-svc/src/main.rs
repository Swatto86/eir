mod ai;
mod ask;
mod audit;
mod config;
mod digest;
mod disk_scan;
mod executor;
mod explain;
mod feedback;
mod learn;
mod models;
mod pipe_server;
mod policy;
mod safety;
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
    /// Escalations performed today — a provider-agnostic backstop, since the USD
    /// budget can't bound providers that report no cost.
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
    /// True while a startup scan is in flight (prevents overlap).
    startup_scan_running: bool,
    /// Server-side map of the last startup scan's entry ids → toggle info, so an
    /// enable/disable click can be reconstructed into a `StartupSet` action server-side
    /// (the wire carries only the opaque id and a bool).
    startup_targets: std::collections::HashMap<String, startup_scan::StartupToggle>,
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
            startup_scan_running: false,
            startup_targets: std::collections::HashMap::new(),
        }
    }
}

/// Restart the service to apply new settings: a detached helper stops then
/// starts EirSvc (LocalSystem — no UAC). It survives this process exiting.
///
/// The helper *waits* for the service to actually reach STOPPED (up to 60s) before
/// starting it, then retries the start until it reports Running. The old fixed ~3s
/// `ping` delay with unconditional `&` chaining could fire `sc start` while the
/// service was still STOP_PENDING (in-flight execution / updater cycle), which fails
/// and leaves the service stopped with nothing to retry.
fn restart_self() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const SCRIPT: &str = "sc.exe stop EirSvc | Out-Null; \
         $d=(Get-Date).AddSeconds(60); \
         while((Get-Date) -lt $d){ \
             if((Get-Service EirSvc -ErrorAction SilentlyContinue).Status -eq 'Stopped'){break}; \
             Start-Sleep -Milliseconds 500 }; \
         for($i=0;$i -lt 5;$i++){ \
             sc.exe start EirSvc | Out-Null; \
             Start-Sleep -Seconds 1; \
             if((Get-Service EirSvc -ErrorAction SilentlyContinue).Status -eq 'Running'){break} }";
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
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
        startup: st.startup.clone(),
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

/// Hard backstop on escalations per UTC day. The USD budget bounds providers
/// that report or estimate cost (OpenRouter's usage chunk, Anthropic's estimated
/// pricing); Kilo Code may report tokens without cost, so this count is the
/// floor guarantee. Always applied so the cap holds regardless of provider.
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
    /// For a successful registry reset, the persisted undo-record id so the UI can
    /// offer a one-click revert. None for everything else.
    undo_id: Option<i64>,
    /// Set to an undo-record id when a user-triggered undo SUCCEEDED, so the loop can
    /// clear that id from the original activity row (its "Undo" button disappears).
    cleared_undo_id: Option<i64>,
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
/// decision loop: each job runs `executor::execute` (panic-isolated) and writes the
/// audit/feedback records, then reports an [`ExecOutcome`] back over `done_tx` for the
/// loop to fold into `SvcState`. Because execution no longer blocks the loop, UI
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
                label,
                diagnosis,
                confidence,
                reason,
            } = job;
            // Per-job backstop timeout. Most actions carry their own timeout (powershell
            // DEFAULT_TIMEOUT, driver/security calls, etc.); this bounds the queue so one
            // wedged action can't latch every later action on "Executing". SFC/DISM are
            // legitimately long, so they get a longer cap than the default.
            let exec_max = exec_timeout(&action);
            // Run the whole job body (execute + audit + feedback) inside an isolated,
            // timeout-bounded task: a panic anywhere is contained to this job (the
            // worker survives to run the next), and a hang can't wedge the queue.
            let db2 = db.clone();
            let label2 = label.clone();
            let mut handle = tokio::spawn(async move {
                let result = executor::execute(&action).await;
                let mut undo_id = None;
                match audit::log_execution(&db2, decision_id, &result).await {
                    Ok(exec_id) => {
                        if let Err(e) = audit::mark_decision_executed(&db2, decision_id).await {
                            error!("Failed to mark decision executed: {e}");
                        }
                        if let Err(e) = feedback::record(
                            &db2,
                            exec_id,
                            &result.action,
                            result.success,
                            &baseline,
                        )
                        .await
                        {
                            error!("Failed to record feedback: {e}");
                        }
                        // Persist the registry-undo snapshot for a successful reset so
                        // the UI can offer a one-click revert.
                        if result.success {
                            if let Some(undo) = &result.undo {
                                match audit::save_registry_undo(&db2, exec_id, undo).await {
                                    Ok(id) => undo_id = Some(id),
                                    Err(e) => error!("Failed to persist registry undo: {e}"),
                                }
                            }
                        }
                    }
                    Err(e) => error!("Failed to log execution: {e}"),
                }
                (result, undo_id)
            });
            let (result, undo_id) = match tokio::time::timeout(exec_max, &mut handle).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(_join)) => (
                    ExecutionResult {
                        action: label2.clone(),
                        success: false,
                        output: "execution task panicked".to_string(),
                        undo: None,
                    },
                    None,
                ),
                Err(_elapsed) => {
                    handle.abort();
                    (
                        ExecutionResult {
                            action: label2.clone(),
                            success: false,
                            output: format!(
                                "execution exceeded {}m and was abandoned",
                                exec_max.as_secs() / 60
                            ),
                            undo: None,
                        },
                        None,
                    )
                }
            };

            // Always report back — even on timeout/panic — so the loop clears the
            // in_flight label and never latches the tray on "Executing". If the loop
            // has gone away the whole service is shutting down, so ignore the error.
            let _ = done_tx.send(ExecOutcome {
                label,
                exec_action: result.action,
                success: result.success,
                output: result.output,
                diagnosis,
                confidence,
                reason,
                undo_id,
                cleared_undo_id: None,
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
#[allow(clippy::too_many_arguments)]
async fn route_user_action(
    st: &mut SvcState,
    pol: &policy::ExecutionPolicy,
    db: &SqlitePool,
    exec_tx: &tokio::sync::mpsc::UnboundedSender<ExecJob>,
    action: FixAction,
    diagnosis: &str,
    reason_label: &str,
) {
    let label = format!("{action:?}");
    if st.in_flight.contains(&label) || st.pending.iter().any(|p| p.info.action == label) {
        info!(action = %label, "User action already queued or executing — skipping duplicate");
        return;
    }
    let decision_id = match audit::log_manual_decision(db, diagnosis).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to log manual decision: {e}");
            return;
        }
    };
    // A zeroed baseline: a user-requested action isn't scored for effectiveness the way
    // an autonomous fix is, so an accurate "before" snapshot isn't needed here.
    let baseline = SystemState::default();
    match pol.evaluate(&action, 1.0) {
        policy::Verdict::Block(reason) => {
            info!(reason = %reason, action = %label, "User action blocked by policy");
            push_problem(st, diagnosis, 1.0, &label, true, false, Some(reason));
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
                    return;
                }
                Err(e) => {
                    warn!("Rate limit check failed — skipping user action to fail safe: {e}");
                    return;
                }
                Ok(false) => {}
            }
            st.in_flight.insert(label.clone());
            if let Err(e) = exec_tx.send(ExecJob {
                action,
                decision_id,
                baseline,
                label: label.clone(),
                diagnosis: diagnosis.to_string(),
                confidence: 1.0,
                reason: Some(reason_label.to_string()),
            }) {
                warn!("Executor worker unavailable: {e}");
                st.in_flight.remove(&label);
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
                target_details: explain::target_details(&action),
                reversible: explanation.reversible,
                created_at: chrono::Utc::now().timestamp(),
            };
            match audit::insert_pending_approval(db, decision_id, &action, &info, &baseline).await {
                Ok(row_id) => {
                    let mut info = info;
                    info.id = row_id as u64;
                    st.pending.push(PendingApproval {
                        info,
                        action,
                        decision_id,
                        baseline,
                    });
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
                }
            }
        }
    }
    st.status = resting_status(st);
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
        // Seed last_run from history so a restart doesn't force an immediate update
        // cycle regardless of the configured schedule (the due-check treats
        // last_run == 0 as "never run").
        st.updater.last_run = recent.iter().map(|r| r.at).max().unwrap_or(0);
        st.updater.recent = recent;
    }
    st.advisor = Some(AdvisorStatus {
        enabled: cfg.advisor.enabled,
        settings: cfg.advisor.to_view(),
        ..Default::default()
    });
    // Seed the advisor's daily spend from the DB so its budget / escalation caps
    // survive a restart (a settings change restarts the service) instead of resetting
    // to zero and letting escalation resume after the day's budget was spent.
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
            text,
            generated_at: at,
        });
    }
    // A bad AI config must NOT kill the service — degrade instead, so the pipe
    // and UI stay alive and the user can fix it in Settings.
    let ai: Option<std::sync::Arc<ai::client::AiClient>> = match ai::client::AiClient::new(&cfg.api)
    {
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
        signals::file_watch::spawn(initial_watch_dirs, trigger_tx.clone());
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
    if ai.is_some() {
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
        Result<(String, Option<CallUsage>), String>,
    )>();
    // On-demand disk scan runs off the loop (blocking walkdir) and reports its ranked
    // result back here.
    let (disk_done_tx, mut disk_done_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<disk_scan::DiskScanResult, String>>();
    // On-demand startup scan (registry/folder enumeration + optional AI classify) runs
    // off the loop and reports back here.
    let (startup_done_tx, mut startup_done_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<startup_scan::StartupScanResult, String>>();
    /// Regenerate the digest weekly.
    const DIGEST_INTERVAL_SECS: i64 = 7 * 24 * 3600;
    let mut analysis_running = false;
    const ANALYSIS_MAX: Duration = Duration::from_secs(10 * 60);
    // The labeller is a small text-only call; bound it well under the analysis cap so
    // a wedged CLI subprocess can't hold the labelling flag for long.
    const LABEL_MAX: Duration = Duration::from_secs(3 * 60);
    // The Tier-2 learned-fact labeller is also an AI call, so it runs off the loop
    // (fire-and-forget). This flag stops it stacking; the stored explanation is
    // surfaced by the next facts_for_view refresh.
    let labelling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cycle_count = 0u64;
    // Reactive-trigger pacing: a trigger *schedules* a reaction (react_at)
    // rather than running one inline — the debounce lets an error burst
    // coalesce into one analysis, the min-gap keeps reactions at most once a
    // minute, and because it's a deadline (not a sleep inside the select arm)
    // UI commands and executor outcomes stay responsive throughout, and a
    // trigger landing inside the gap is deferred, never dropped.
    const REACTIVE_DEBOUNCE: Duration = Duration::from_secs(10);
    const REACTIVE_MIN_GAP: Duration = Duration::from_secs(60);
    let mut last_cycle_at = tokio::time::Instant::now();
    let mut react_at: Option<tokio::time::Instant> = None;
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
                    Some(result) = startup_done_rx.recv() => {
                        // A startup scan finished. Fold in the entries + the id→toggle map.
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
                    Some((question, result)) = ask_done_rx.recv() => {
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
                                    answer,
                                    at: chrono::Utc::now().timestamp(),
                                });
                                refresh_ask(&mut st, None);
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
                        st.in_flight.remove(&outcome.label);
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
                                st.status = "Error".to_string();
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
                        st.error = None;

                        // If the UTC day rolled over WHILE this analysis was in flight, the
                        // per-cycle rollover (bottom of the loop body) never ran — it's gated
                        // behind `if analysis_running`. Folding the task's totals back in would
                        // restore yesterday's (possibly near-budget) spend onto the new day and
                        // wrongly block a legitimate new-day escalation. Detect the straddle here
                        // and attribute only THIS analysis's own escalation to the new day.
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
                        // Persist the day's escalation spend so the budget/count caps
                        // survive a restart (see load at startup).
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
                                    if st.in_flight.contains(&label)
                                        || st.pending.iter().any(|p| p.info.action == label)
                                    {
                                        info!(action = %label, "Already executing or awaiting approval — skipping duplicate");
                                        continue;
                                    }
                                    info!(action = ?action, "AUTO-EXECUTING (queued)");
                                    // Hand off to the executor worker and move on; the outcome
                                    // is folded in when it finishes (see exec_done_rx arm).
                                    st.in_flight.insert(label.clone());
                                    if let Err(e) = exec_tx.send(ExecJob {
                                        action: action.clone(),
                                        decision_id,
                                        baseline: snapshot.system_state.clone(),
                                        label: label.clone(),
                                        diagnosis: problem.diagnosis.clone(),
                                        confidence: problem.confidence,
                                        reason: None,
                                    }) {
                                        // The worker is gone; no ExecOutcome will arrive, so
                                        // drop the label instead of leaking it in in_flight
                                        // (which would skip this action forever and pin the
                                        // UI to "Executing").
                                        warn!("Executor worker unavailable: {e}");
                                        st.in_flight.remove(&label);
                                    }
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
                                        if let Err(e) = exec_tx.send(ExecJob {
                                            action: pa.action,
                                            decision_id: pa.decision_id,
                                            baseline: pa.baseline,
                                            label: label.clone(),
                                            diagnosis: pa.info.diagnosis.clone(),
                                            confidence: pa.info.confidence,
                                            reason: Some("approved by user".into()),
                                        }) {
                                            // Worker gone — don't leak the label in in_flight.
                                            warn!("Executor worker unavailable: {e}");
                                            st.in_flight.remove(&label);
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
                            // The pipe is writable by any authenticated local user, so cap
                            // the ignore list (far above any real installed-app count) and
                            // only persist on an ACTUAL change — otherwise a flood of
                            // distinct ids could grow config.toml without bound and hammer
                            // the LocalSystem config with a synchronous write per message.
                            const MAX_IGNORED: usize = 500;
                            let key = id.to_lowercase();
                            let mut changed = false;
                            if ignore {
                                let present = cfg
                                    .updater
                                    .ignored
                                    .iter()
                                    .any(|x| x.eq_ignore_ascii_case(&key));
                                if !present {
                                    if cfg.updater.ignored.len() >= MAX_IGNORED {
                                        warn!(
                                            "Ignore list at cap ({MAX_IGNORED}) — refusing to add '{key}'"
                                        );
                                    } else {
                                        cfg.updater.ignored.push(key.clone());
                                        changed = true;
                                    }
                                }
                            } else if cfg.updater.ignored.iter().any(|x| x.eq_ignore_ascii_case(&key))
                            {
                                cfg.updater.ignored.retain(|x| !x.eq_ignore_ascii_case(&key));
                                changed = true;
                            }
                            // A blank note means "unchanged" — never "clear". The UI has
                            // no notes editor and always sends an empty note with an
                            // ignore toggle, so treating blank as delete would wipe a
                            // note hand-set in config.toml on every Ignore/Unignore click.
                            let n = note.trim();
                            if !n.is_empty()
                                && cfg.updater.notes.get(&key).map(String::as_str) != Some(n)
                            {
                                cfg.updater.notes.insert(key.clone(), n.to_string());
                                changed = true;
                            }
                            if changed {
                                if let Err(e) = config::save(&cfg, "config.toml") {
                                    warn!("Failed to save app note: {e}");
                                }
                            }
                            // Reflect the toggle on the live row so the UI shows it
                            // immediately — the broadcast below carries the unchanged
                            // apps list from the last cycle, so without this the
                            // optimistic dim would revert on the next poll.
                            for a in st.updater.apps.iter_mut() {
                                if a.id.eq_ignore_ascii_case(&key) {
                                    a.ignored = ignore;
                                }
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
                        UiMsg::UndoRegistry { id } => {
                            // Restore a prior registry value off the loop, reporting the
                            // outcome through the exec_done arm so it appears in the
                            // activity feed and never blocks ui_rx on the PowerShell call.
                            let db_u = db.clone();
                            let done = undo_done_tx.clone();
                            tokio::spawn(async move {
                                // CLAIM atomically so two concurrent undo clicks can't both
                                // run (the second sees the record already claimed).
                                match audit::claim_registry_undo(&db_u, id).await {
                                    Ok(Some(undo)) => {
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
                                        });
                                    }
                                    Ok(None) => {
                                        warn!("Registry undo {id} not found or already undone");
                                    }
                                    Err(e) => error!("Failed to load registry undo {id}: {e}"),
                                }
                            });
                        }
                        UiMsg::AskEir { question } => {
                            let now = chrono::Utc::now().timestamp();
                            if let Some(reason) = ask::ask_rejection_reason(
                                &question,
                                ai.is_some(),
                                st.ask_running,
                                st.last_ask_at,
                                now,
                            ) {
                                refresh_ask(&mut st, Some(reason.to_string()));
                                pipe.broadcast_status(build_status(&st));
                            } else if let Some(ai_ref) = ai.as_ref() {
                                st.ask_running = true;
                                refresh_ask(&mut st, None);
                                pipe.broadcast_status(build_status(&st));
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
                                tokio::spawn(async move {
                                    // Bound the whole answer in a nested task + timeout,
                                    // mirroring the analysis/digest tasks, so a wedged AI
                                    // call can't latch ask_running for the process lifetime.
                                    const ASK_MAX: Duration = Duration::from_secs(4 * 60);
                                    let q = question.clone();
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
                                        let prompt = ask::build_prompt(&ctx, &question);
                                        // Main/default model, no web search — the labeller/
                                        // digest completion entry point.
                                        ai_a.complete_text(&prompt, "")
                                            .await
                                            .map(|(t, u)| (t.trim().to_string(), u))
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
                                                inner.abort();
                                                Err("answering timed out".to_string())
                                            }
                                        };
                                    let _ = done.send((q, result));
                                });
                            }
                        }
                        UiMsg::ScanDisk => {
                            // Gate on the scan-in-flight flag and pause (a paused guardian
                            // shouldn't be walking the disk). Keep prior results visible
                            // while the new scan runs.
                            if !st.disk_scan_running && !st.paused {
                                st.disk_scan_running = true;
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
                                    const SCAN_MAX: Duration = Duration::from_secs(5 * 60);
                                    // The scan bounds its own walk with this deadline; the
                                    // join timeout is a backstop (spawn_blocking can't be
                                    // aborted, but the deadline stops the walk regardless).
                                    let deadline = std::time::Instant::now() + SCAN_MAX;
                                    let mut handle =
                                        tokio::task::spawn_blocking(move || disk_scan::scan(deadline));
                                    let result = match tokio::time::timeout(
                                        SCAN_MAX + Duration::from_secs(30),
                                        &mut handle,
                                    )
                                    .await
                                    {
                                        Ok(Ok(res)) => Ok(res),
                                        Ok(Err(_join)) => Err("disk scan task panicked".to_string()),
                                        Err(_elapsed) => {
                                            handle.abort();
                                            Err("disk scan timed out".to_string())
                                        }
                                    };
                                    let _ = done.send(result);
                                });
                            }
                        }
                        UiMsg::CleanDiskEntry { id } => {
                            // Map the opaque id to a safe action from THIS service's own
                            // last scan (unknown/stale id → ignore), then route it through
                            // the normal policy gate. The wire never carries an action.
                            match st.disk_targets.get(&id).cloned() {
                                Some(action) => {
                                    route_user_action(
                                        &mut st,
                                        &pol,
                                        &db,
                                        &exec_tx,
                                        action,
                                        "User-requested disk cleanup",
                                        "user-requested cleanup",
                                    )
                                    .await;
                                    pipe.broadcast_status(build_status(&st));
                                }
                                None => {
                                    info!(%id, "Clean requested for unknown/stale disk entry — ignoring");
                                }
                            }
                        }
                        UiMsg::ScanStartup => {
                            if !st.startup_scan_running && !st.paused {
                                st.startup_scan_running = true;
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
                                    const STARTUP_MAX: Duration = Duration::from_secs(4 * 60);
                                    let mut inner = tokio::spawn(async move {
                                        startup_scan::scan(ai_s.as_deref(), &model_s)
                                            .await
                                            .map_err(|e| e.to_string())
                                    });
                                    let result =
                                        match tokio::time::timeout(STARTUP_MAX, &mut inner).await {
                                            Ok(Ok(r)) => r,
                                            Ok(Err(_join)) => {
                                                Err("startup scan task panicked".to_string())
                                            }
                                            Err(_elapsed) => {
                                                inner.abort();
                                                Err("startup scan timed out".to_string())
                                            }
                                        };
                                    let _ = done.send(result);
                                });
                            }
                        }
                        UiMsg::SetStartupEntry { id, enable } => {
                            // Reconstruct the toggle from THIS service's own last scan
                            // (unknown/stale id → ignore), then route the StartupSet through
                            // the normal policy gate (approval-gated — reversible, but the
                            // pipe is writable by any local user, so a human confirms).
                            let target = st
                                .startup_targets
                                .get(&id)
                                .map(|t| (t.name.clone(), t.location.clone(), t.hive.clone()));
                            match target {
                                Some((name, location, hive)) => {
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
                                    route_user_action(
                                        &mut st,
                                        &pol,
                                        &db,
                                        &exec_tx,
                                        action,
                                        diag,
                                        "user startup change",
                                    )
                                    .await;
                                    pipe.broadcast_status(build_status(&st));
                                }
                                None => {
                                    info!(%id, "Startup toggle for unknown/stale entry — ignoring");
                                }
                            }
                        }
                        }
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
                {
                    let sys = &snapshot.system_state;
                    st.cpu             = sys.cpu_usage_percent;
                    st.memory          = sys.memory_usage_percent;
                    st.disk            = sys.disk_usage_percent;
                    st.failed_services = sys.failed_services.clone();
                }
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
                                inner.abort();
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
                            inner.abort();
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
                            inner.abort();
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

    // Drain the executor before the runtime is dropped. Both exit paths land here: a
    // settings-save `restart_self()` returns out of the loop block, and a shutdown/SCM
    // stop (including the self-updater's `sc stop`) cancels it. An approved fix that was
    // queued or executing gets to finish and log (execution_log / undo snapshot / feedback)
    // instead of being aborted when the runtime drops — PROVIDED it completes within the
    // drain window. Dropping the only `ExecJob` sender closes the job channel so the worker
    // stops once its queue is empty; the idle case joins instantly. The 30s cap matches
    // SCM's own stop timeout (run_service reports StopPending + a 35s wait_hint so SCM
    // waits): a genuinely long repair (SFC/DISM, tens of minutes) can still be cut off by
    // SCM force-killing the process, which no in-process drain can outlast — those remain
    // best-effort, not guaranteed.
    drop(exec_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), exec_handle).await;
    info!("Executor drained — service loop stopped");
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
