mod ai;
mod approval;
mod audit;
mod config;
mod executor;
mod models;
mod signals;

use anyhow::Result;
use models::SignalSnapshot;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

const CONFIDENCE_THRESHOLD: f32 = 0.80;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::load("config.toml")?;

    let log_level = cfg.logging.level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    info!("Starting Sentry v0.2 (Phase 2 — Manual Approval)");

    let db = audit::init_db(&cfg.persistence.audit_db).await?;
    let ai = ai::client::AiClient::new(&cfg.api)?;

    let (event_log_shared, _el_shutdown) = signals::event_log::spawn(
        cfg.monitoring.event_log_channels.clone(),
        cfg.monitoring.event_log_poll_interval_secs,
    );
    let (file_watch_shared, _fw_shutdown) =
        signals::file_watch::spawn(cfg.monitoring.log_directories.clone());
    let (wmi_shared, _wmi_shutdown) =
        signals::wmi::spawn(cfg.monitoring.wmi_poll_interval_secs);

    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut decision_ticker =
        interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));

    info!(
        interval_secs = cfg.monitoring.decision_interval_secs,
        "Decision loop started"
    );

    loop {
        decision_ticker.tick().await;

        let history = audit::get_recent_decisions(&db, 5).await.unwrap_or_else(|e| {
            warn!("Failed to load decision history: {e}");
            vec![]
        });

        let snapshot = SignalSnapshot {
            timestamp: chrono::Utc::now(),
            event_log: signals::event_log::snapshot(&event_log_shared),
            file_changes: signals::file_watch::drain(&file_watch_shared),
            system_state: signals::wmi::current(&wmi_shared),
            decision_history: history.clone(),
        };

        info!(
            event_entries = snapshot.event_log.len(),
            file_changes = snapshot.file_changes.len(),
            "Signal snapshot collected"
        );

        let claude_decision = match ai.analyze(&snapshot, &history).await {
            Ok(d) => d,
            Err(e) => {
                error!("Claude analysis failed: {e}");
                continue;
            }
        };

        let decision_id = match audit::log_decision(&db, &snapshot, &claude_decision, false).await {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to write audit log: {e}");
                continue;
            }
        };

        for problem in &claude_decision.problems {
            info!(
                confidence = problem.confidence,
                diagnosis = %problem.diagnosis,
                "Problem identified"
            );

            if problem.confidence < CONFIDENCE_THRESHOLD {
                info!(
                    confidence = problem.confidence,
                    threshold = CONFIDENCE_THRESHOLD,
                    "Skipping — confidence below threshold"
                );
                continue;
            }

            let Some(action) = problem.parse_fix_action() else {
                warn!(
                    fix = %problem.proposed_fix,
                    "Proposed fix has unknown action type — skipping"
                );
                continue;
            };

            match approval::prompt(problem, &action).await {
                approval::Decision::Approved => {
                    let result = executor::execute(&action).await;
                    if let Err(e) = audit::log_execution(&db, decision_id, &result).await {
                        error!("Failed to log execution: {e}");
                    }
                }
                approval::Decision::Rejected => {
                    info!(diagnosis = %problem.diagnosis, "Fix rejected by user");
                }
                approval::Decision::Skipped => {
                    info!(diagnosis = %problem.diagnosis, "Fix skipped");
                }
            }
        }
    }
}
