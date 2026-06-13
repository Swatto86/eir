use crate::models::FixAction;
use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// Returns true if this exact action was successfully executed within the rate-limit window.
/// Uses the same Debug format stored in execution_log.action for an exact match.
pub async fn rate_limited(pool: &SqlitePool, action: &FixAction, window_mins: u32) -> Result<bool> {
    let key = format!("{action:?}");
    let cutoff = (Utc::now() - chrono::Duration::minutes(window_mins as i64)).to_rfc3339();

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM execution_log WHERE action = ? AND executed_at > ? AND success = 1",
    )
    .bind(&key)
    .bind(&cutoff)
    .fetch_one(pool)
    .await?;

    Ok(count > 0)
}

/// Overall success rate across all executions. Returns 1.0 when no data.
pub async fn success_rate(pool: &SqlitePool) -> Result<f32> {
    let row = sqlx::query("SELECT SUM(success), COUNT(*) FROM execution_log")
        .fetch_one(pool)
        .await?;
    let successes: Option<i64> = row.try_get(0)?;
    let total: i64 = row.try_get(1)?;
    if total == 0 {
        return Ok(1.0);
    }
    Ok(successes.unwrap_or(0) as f32 / total as f32)
}
