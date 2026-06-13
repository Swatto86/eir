use crate::models::SystemState;
use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// Record the "before" state at the time of execution. The next cycle fills in "after".
pub async fn record(
    pool: &SqlitePool,
    execution_log_id: i64,
    action: &str,
    succeeded: bool,
    state: &SystemState,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO execution_feedback
         (execution_log_id, action, succeeded, cpu_before, memory_before,
          failed_services_before, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(execution_log_id)
    .bind(action)
    .bind(if succeeded { 1i64 } else { 0i64 })
    .bind(state.cpu_usage_percent as f64)
    .bind(state.memory_usage_percent as f64)
    .bind(state.failed_services.len() as i64)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fill in "after" metrics for all feedback rows still missing them.
/// Call at the start of each decision cycle once signals are collected.
pub async fn update_after_states(pool: &SqlitePool, state: &SystemState) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, cpu_before, memory_before, failed_services_before
         FROM execution_feedback
         WHERE cpu_after IS NULL
         ORDER BY id DESC LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    let cpu_after = state.cpu_usage_percent as f64;
    let mem_after = state.memory_usage_percent as f64;
    let fs_after = state.failed_services.len() as i64;

    for row in rows {
        let id: i64 = row.try_get("id")?;
        let cpu_before: Option<f64> = row.try_get("cpu_before")?;
        let mem_before: Option<f64> = row.try_get("memory_before")?;
        let fs_before: Option<i64> = row.try_get("failed_services_before")?;
        let score = improvement_score(cpu_before, mem_before, fs_before, cpu_after, mem_after, fs_after);

        sqlx::query(
            "UPDATE execution_feedback
             SET cpu_after = ?, memory_after = ?, failed_services_after = ?,
                 improvement_score = ?
             WHERE id = ?",
        )
        .bind(cpu_after)
        .bind(mem_after)
        .bind(fs_after)
        .bind(score)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn improvement_score(
    cpu_before: Option<f64>,
    mem_before: Option<f64>,
    fs_before: Option<i64>,
    cpu_after: f64,
    mem_after: f64,
    fs_after: i64,
) -> f64 {
    // Positive score = system improved; negative = degraded.
    let cpu_delta = cpu_before.map(|b| b - cpu_after).unwrap_or(0.0); // CPU drop is good
    let mem_delta = mem_before.map(|b| b - mem_after).unwrap_or(0.0); // memory drop is good
    let fs_delta = fs_before
        .map(|b| (b - fs_after) as f64)
        .unwrap_or(0.0); // fewer failed services is good

    cpu_delta * 0.3 + mem_delta * 0.3 + fs_delta * 10.0
}

/// Human-readable summary of recent execution outcomes for the AI prompt.
pub async fn recent_summary(pool: &SqlitePool, limit: i64) -> Result<String> {
    let rows = sqlx::query(
        "SELECT action, succeeded, improvement_score, recorded_at
         FROM execution_feedback
         ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok("No execution history yet.".to_string());
    }

    let mut lines = Vec::new();
    for row in rows {
        let action: String = row.try_get("action")?;
        let succeeded: i64 = row.try_get("succeeded")?;
        let improvement: Option<f64> = row.try_get("improvement_score")?;
        let ts: String = row.try_get("recorded_at")?;
        let short_ts = &ts[..ts.len().min(16)];

        let outcome = if succeeded != 0 { "SUCCESS" } else { "FAILURE" };
        let delta_str = match improvement {
            Some(s) if s > 1.0 => format!(", improved (+{s:.1})"),
            Some(s) if s < -1.0 => format!(", degraded ({s:.1})"),
            Some(_) => ", no measurable change".to_string(),
            None => " (pending next cycle measurement)".to_string(),
        };
        lines.push(format!("- {short_ts}: {action} -> {outcome}{delta_str}"));
    }

    Ok(lines.join("\n"))
}
