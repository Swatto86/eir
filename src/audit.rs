use crate::models::{ClaudeDecision, ExecutionResult, PastDecision, SignalSnapshot};
use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;
use tracing::info;

pub async fn init_db(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{path}?mode=rwc"))?
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    info!("Audit database initialised at {path}");
    Ok(pool)
}

pub async fn log_decision(
    pool: &SqlitePool,
    snapshot: &SignalSnapshot,
    decision: &ClaudeDecision,
    executed: bool,
) -> Result<i64> {
    let timestamp = Utc::now().to_rfc3339();
    let snapshot_json = serde_json::to_string(snapshot)?;
    let response_json = serde_json::to_string(decision)?;
    let max_confidence = decision
        .problems
        .iter()
        .map(|p| p.confidence)
        .fold(f32::NEG_INFINITY, f32::max);
    let executed_int: i64 = if executed { 1 } else { 0 };

    let id = sqlx::query(
        "INSERT INTO decisions (timestamp, signal_snapshot, claude_response, confidence, executed)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&timestamp)
    .bind(&snapshot_json)
    .bind(&response_json)
    .bind(max_confidence as f64)
    .bind(executed_int)
    .execute(pool)
    .await?
    .last_insert_rowid();

    let state = &snapshot.system_state;
    let failed_count = state.failed_services.len() as i64;
    let state_json = serde_json::to_string(state)?;

    sqlx::query(
        "INSERT INTO system_state_history
         (timestamp, cpu_usage, memory_usage, disk_usage, failed_services_count, snapshot)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&timestamp)
    .bind(state.cpu_usage_percent as f64)
    .bind(state.memory_usage_percent as f64)
    .bind(state.disk_usage_percent as f64)
    .bind(failed_count)
    .bind(&state_json)
    .execute(pool)
    .await?;

    Ok(id)
}

pub async fn get_recent_decisions(pool: &SqlitePool, limit: i64) -> Result<Vec<PastDecision>> {
    let rows = sqlx::query(
        "SELECT timestamp, claude_response, confidence FROM decisions ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut decisions = Vec::new();
    for row in rows {
        let ts_str: String = row.try_get("timestamp")?;
        let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let response_str: String = row.try_get("claude_response")?;
        let confidence: f64 = row.try_get("confidence")?;

        let response: ClaudeDecision = serde_json::from_str(&response_str).unwrap_or_else(|_| {
            ClaudeDecision {
                analysis: String::new(),
                problems: vec![],
            }
        });

        let diagnosis = response
            .problems
            .first()
            .map(|p| p.diagnosis.clone())
            .unwrap_or_else(|| response.analysis.clone());

        let fix_proposed = response
            .problems
            .first()
            .map(|p| serde_json::to_string(&p.proposed_fix).unwrap_or_default())
            .unwrap_or_default();

        decisions.push(PastDecision {
            timestamp: ts,
            diagnosis,
            confidence: confidence as f32,
            fix_proposed,
        });
    }

    Ok(decisions)
}

pub async fn log_execution(
    pool: &SqlitePool,
    decision_id: i64,
    result: &ExecutionResult,
) -> anyhow::Result<i64> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let id = sqlx::query(
        "INSERT INTO execution_log (decision_id, action, success, output, executed_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(decision_id)
    .bind(&result.action)
    .bind(if result.success { 1i64 } else { 0i64 })
    .bind(&result.output)
    .bind(&timestamp)
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}
