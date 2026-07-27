use crate::models::FixAction;
use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// Consecutive failures of the same action within the window before it is
/// suppressed until the window rolls over — the circuit breaker that stops a
/// persistently failing auto-fix from retrying every single cycle.
const FAILURE_BREAKER_THRESHOLD: i64 = 3;

/// Returns true if this exact action should NOT run again yet:
/// - it already **succeeded** within the rate-limit window (no point re-running
///   an applied fix), or
/// - it **failed ≥ 3 times** within the window (circuit breaker — back off
///   until the window rolls over instead of failing on every cycle).
///
/// New rows use `FixAction::dedup_key`; legacy rows without that additive column
/// still match their exact Debug action.
pub async fn rate_limited(pool: &SqlitePool, action: &FixAction, window_mins: u32) -> Result<bool> {
    let legacy_key = format!("{action:?}");
    let semantic_key = action.dedup_key();
    let cutoff = (Utc::now() - chrono::Duration::minutes(i64::from(window_mins))).to_rfc3339();

    let row = sqlx::query(
        "SELECT COALESCE(SUM(success), 0), COUNT(*) FROM execution_log \
         WHERE executed_at > ?
           AND (action_key = ? OR (action_key IS NULL AND action = ?))",
    )
    .bind(&cutoff)
    .bind(&semantic_key)
    .bind(&legacy_key)
    .fetch_one(pool)
    .await?;
    let successes: i64 = row.try_get(0)?;
    let total: i64 = row.try_get(1)?;
    let failures = total - successes;

    Ok(successes > 0 || failures >= FAILURE_BREAKER_THRESHOLD)
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool(tag: &str) -> SqlitePool {
        let path = std::env::temp_dir().join(format!("eir-safety-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!(
            "sqlite:{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.expect("open db");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        sqlx::query(
            "INSERT INTO decisions (timestamp, signal_snapshot, claude_response, confidence) \
             VALUES ('2026-01-01T00:00:00Z', '{}', '{}', 0.9)",
        )
        .execute(&pool)
        .await
        .expect("seed decision");
        pool
    }

    async fn log_exec(pool: &SqlitePool, action: &FixAction, success: bool) {
        sqlx::query(
            "INSERT INTO execution_log \
             (decision_id, action, action_key, success, output, executed_at) \
             VALUES (1, ?, ?, ?, '', ?)",
        )
        .bind(format!("{action:?}"))
        .bind(action.dedup_key())
        .bind(success as i64)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await
        .expect("insert execution");
    }

    #[tokio::test]
    async fn success_within_window_rate_limits() {
        let pool = test_pool("success").await;
        let action = FixAction::ServiceRestart {
            service_name: "Spooler".into(),
        };
        assert!(!rate_limited(&pool, &action, 60).await.unwrap());
        log_exec(&pool, &action, true).await;
        assert!(rate_limited(&pool, &action, 60).await.unwrap());
    }

    #[tokio::test]
    async fn repeated_failures_trip_the_breaker() {
        let pool = test_pool("breaker").await;
        let action = FixAction::ServiceRestart {
            service_name: "Broken".into(),
        };
        // 1–2 failures: retry allowed. 3rd failure: breaker trips.
        log_exec(&pool, &action, false).await;
        assert!(!rate_limited(&pool, &action, 60).await.unwrap());
        log_exec(&pool, &action, false).await;
        assert!(!rate_limited(&pool, &action, 60).await.unwrap());
        log_exec(&pool, &action, false).await;
        assert!(rate_limited(&pool, &action, 60).await.unwrap());
        // A different semantic target is unaffected.
        let other = FixAction::ServiceRestart {
            service_name: "Healthy".into(),
        };
        assert!(!rate_limited(&pool, &other, 60).await.unwrap());
    }

    #[tokio::test]
    async fn regenerated_parameters_share_the_semantic_rate_limit() {
        let pool = test_pool("semantic").await;
        let first = FixAction::LogCleanup {
            path: r"C:\Logs".into(),
            days_old: 30,
        };
        let regenerated = FixAction::LogCleanup {
            path: r"c:\logs".into(),
            days_old: 7,
        };
        log_exec(&pool, &first, true).await;
        assert!(rate_limited(&pool, &regenerated, 60).await.unwrap());
    }

    #[tokio::test]
    async fn legacy_execution_rows_still_rate_limit_exact_actions() {
        let pool = test_pool("legacy").await;
        let action = FixAction::ServiceRestart {
            service_name: "Spooler".into(),
        };
        sqlx::query(
            "INSERT INTO execution_log
             (decision_id, action, success, output, executed_at)
             VALUES (1, ?, 1, '', ?)",
        )
        .bind(format!("{action:?}"))
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .expect("insert legacy execution");
        assert!(rate_limited(&pool, &action, 60).await.unwrap());
    }
}
