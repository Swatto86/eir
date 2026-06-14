use crate::models::{CallUsage, ClaudeDecision, ExecutionResult, PastDecision, SignalSnapshot};
use anyhow::Result;
use chrono::Utc;
use eir_proto::UsageSummary;
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use std::str::FromStr;
use tracing::info;

pub async fn init_db(path: &str) -> Result<SqlitePool> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite:{path}?mode=rwc"))?.create_if_missing(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::migrate!("../migrations").run(&pool).await?;
    info!("Audit database initialised at {path}");
    Ok(pool)
}

pub async fn log_decision(
    pool: &SqlitePool,
    snapshot: &SignalSnapshot,
    decision: &ClaudeDecision,
) -> Result<i64> {
    let timestamp = Utc::now().to_rfc3339();
    let snapshot_json = serde_json::to_string(snapshot)?;
    let response_json = serde_json::to_string(decision)?;
    let max_confidence = decision
        .problems
        .iter()
        .map(|p| p.confidence)
        .fold(0f32, f32::max);

    let id = sqlx::query(
        "INSERT INTO decisions (timestamp, signal_snapshot, claude_response, confidence, executed)
         VALUES (?, ?, ?, ?, 0)",
    )
    .bind(&timestamp)
    .bind(&snapshot_json)
    .bind(&response_json)
    .bind(max_confidence as f64)
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

pub async fn mark_decision_executed(pool: &SqlitePool, decision_id: i64) -> Result<()> {
    sqlx::query("UPDATE decisions SET executed = 1 WHERE id = ?")
        .bind(decision_id)
        .execute(pool)
        .await?;
    Ok(())
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

        let response: ClaudeDecision =
            serde_json::from_str(&response_str).unwrap_or_else(|_| ClaudeDecision {
                analysis: String::new(),
                problems: vec![],
            });

        if response.problems.is_empty() {
            decisions.push(PastDecision {
                timestamp: ts,
                diagnosis: response.analysis.clone(),
                confidence: confidence as f32,
                fix_proposed: String::new(),
            });
        } else {
            for p in &response.problems {
                decisions.push(PastDecision {
                    timestamp: ts,
                    diagnosis: p.diagnosis.clone(),
                    confidence: p.confidence,
                    fix_proposed: serde_json::to_string(&p.proposed_fix).unwrap_or_default(),
                });
            }
        }
    }

    Ok(decisions)
}

pub async fn log_usage(pool: &SqlitePool, usage: &CallUsage) -> Result<()> {
    let ts = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO usage_log
         (timestamp, input_tokens, output_tokens, cache_creation, cache_read, cost_usd)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&ts)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_creation as i64)
    .bind(usage.cache_read as i64)
    .bind(usage.cost_usd)
    .execute(pool)
    .await?;
    Ok(())
}

/// Aggregate Claude usage over the last 24 hours and 7 days.
pub async fn usage_summary(pool: &SqlitePool) -> Result<UsageSummary> {
    async fn agg(pool: &SqlitePool, cutoff: &str) -> Result<(u64, u64, f64)> {
        let row = sqlx::query(
            "SELECT COUNT(*),
                    COALESCE(SUM(input_tokens + output_tokens + cache_creation + cache_read), 0),
                    COALESCE(SUM(cost_usd), 0)
             FROM usage_log WHERE timestamp > ?",
        )
        .bind(cutoff)
        .fetch_one(pool)
        .await?;
        let calls: i64 = row.try_get(0)?;
        let tokens: i64 = row.try_get(1)?;
        let cost: f64 = row.try_get(2)?;
        Ok((calls as u64, tokens as u64, cost))
    }

    let now = Utc::now();
    let day_cutoff = (now - chrono::Duration::hours(24)).to_rfc3339();
    let week_cutoff = (now - chrono::Duration::days(7)).to_rfc3339();
    let (calls_today, tokens_today, cost_today_usd) = agg(pool, &day_cutoff).await?;
    let (calls_week, tokens_week, cost_week_usd) = agg(pool, &week_cutoff).await?;

    Ok(UsageSummary {
        calls_today,
        calls_week,
        tokens_today,
        tokens_week,
        cost_today_usd,
        cost_week_usd,
    })
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
