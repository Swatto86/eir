mod ai;
mod audit;
mod config;
mod executor;
mod models;
mod signals;

use anyhow::Result;
use models::SignalSnapshot;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::load("config.toml")?;

    let log_level = cfg.logging.level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    info!("Starting Sentry v0.1 (Phase 1 — Analysis Only)");

    let db = audit::init_db(&cfg.persistence.audit_db).await?;
    let ai = ai::client::AiClient::new(&cfg.api.anthropic_api_key, &cfg.api.model);

    let (event_log_shared, _el_shutdown) = signals::event_log::spawn(
        cfg.monitoring.event_log_channels.clone(),
        cfg.monitoring.event_log_poll_interval_secs,
    );

    let (file_watch_shared, _fw_shutdown) =
        signals::file_watch::spawn(cfg.monitoring.log_directories.clone());

    let (wmi_shared, _wmi_shutdown) =
        signals::wmi::spawn(cfg.monitoring.wmi_poll_interval_secs);

    // Give signal tasks an initial tick before first decision
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut decision_ticker =
        interval(Duration::from_secs(cfg.monitoring.decision_interval_secs));

    info!(
        interval_secs = cfg.monitoring.decision_interval_secs,
        "Decision loop started"
    );

    loop {
        decision_ticker.tick().await;

        let history = match audit::get_recent_decisions(&db, 5).await {
            Ok(h) => h,
            Err(e) => {
                warn!("Failed to load decision history: {e}");
                vec![]
            }
        };

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

        match ai.analyze(&snapshot, &history).await {
            Ok(decision) => {
                executor::log_proposed(&decision);
                if let Err(e) = audit::log_decision(&db, &snapshot, &decision, false).await {
                    error!("Failed to write audit log: {e}");
                }
            }
            Err(e) => {
                error!("Claude analysis failed: {e}");
            }
        }
    }
}
