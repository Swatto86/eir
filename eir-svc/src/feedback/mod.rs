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
    // Fill EVERY row still awaiting an "after" measurement. A `LIMIT` here stranded
    // rows beyond the newest N permanently at NULL (excluded from effectiveness
    // learning); executions per cycle are few, so the pending set stays small.
    let rows = sqlx::query(
        "SELECT id, cpu_before, memory_before, failed_services_before
         FROM execution_feedback
         WHERE cpu_after IS NULL
         ORDER BY id DESC",
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
        let score = improvement_score(
            cpu_before, mem_before, fs_before, cpu_after, mem_after, fs_after,
        );

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

/// Delete feedback rows older than `days`. The detectors only look back 30 days, so
/// anything well past that is dead weight — without this the table grows unbounded on
/// a long-lived install and slowly slows the windowed queries.
pub async fn prune_old(pool: &SqlitePool, days: i64) -> Result<u64> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let res = sqlx::query("DELETE FROM execution_feedback WHERE recorded_at < ?")
        .bind(&cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
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
    let fs_delta = fs_before.map(|b| (b - fs_after) as f64).unwrap_or(0.0); // fewer failed services is good

    cpu_delta * 0.3 + mem_delta * 0.3 + fs_delta * 10.0
}

/// Collapse a fix's raw output into a single short clause for the AI prompt:
/// whitespace-normalised and capped, so a failure reason is visible without
/// dumping a multi-line stack trace into every future prompt.
fn condense_reason(output: &str) -> Option<String> {
    let one_line = output.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    const CAP: usize = 200;
    let reason = if one_line.chars().count() > CAP {
        format!("{}…", one_line.chars().take(CAP).collect::<String>())
    } else {
        one_line
    };
    Some(reason)
}

/// Human-readable summary of recent execution outcomes for the AI prompt. For
/// FAILUREs it includes the actual error text (joined from execution_log) so the
/// model can reason about *why* a fix failed — not just that it did — and pick a
/// different remedy next cycle instead of re-proposing the same failing action.
pub async fn recent_summary(pool: &SqlitePool, limit: i64) -> Result<String> {
    let rows = sqlx::query(
        "SELECT f.action, f.succeeded, f.improvement_score, f.recorded_at, e.output
         FROM execution_feedback f
         LEFT JOIN execution_log e ON e.id = f.execution_log_id
         ORDER BY f.id DESC LIMIT ?",
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
        let output: Option<String> = row.try_get("output")?;
        let short_ts = &ts[..ts.len().min(16)];

        let succeeded = succeeded != 0;
        let outcome = if succeeded { "SUCCESS" } else { "FAILURE" };
        let delta_str = match improvement {
            Some(s) if s > 1.0 => format!(", improved (+{s:.1})"),
            Some(s) if s < -1.0 => format!(", degraded ({s:.1})"),
            Some(_) => ", no measurable change".to_string(),
            None => " (pending next cycle measurement)".to_string(),
        };
        // The error text is what the model needs to avoid repeating a bad fix.
        let reason = if succeeded {
            String::new()
        } else {
            output
                .as_deref()
                .and_then(condense_reason)
                .map(|r| format!(" [reason: {r}]"))
                .unwrap_or_default()
        };
        lines.push(format!(
            "- {short_ts}: {action} -> {outcome}{delta_str}{reason}"
        ));
    }

    Ok(lines.join("\n"))
}
