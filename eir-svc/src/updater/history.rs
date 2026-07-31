//! Persist every update attempt to the audit DB (the `update_attempts` table from
//! migration 0007). Follows the same bare-query/bind pattern as `audit.rs`. This is
//! the audit trail for unattended installs and the data the UI's history view reads.

use crate::updater::domain::{AttemptOutcome, ErrorCategory, UpdateCandidate};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

const HISTORY_CLEARED_AT_KEY: &str = "updater_history_cleared_at";
const LAST_CLEAN_RUN_KEY: &str = "updater_last_clean_run";
const LAST_CYCLE_STATUS_KEY: &str = "updater_last_cycle_status";
const LAST_RUN_KEY: &str = "updater_last_run";

#[derive(Serialize, Deserialize)]
struct StoredCycleStatus {
    at: i64,
    notes: Vec<String>,
    apps: Vec<eir_proto::UpdaterAppRow>,
}

/// The failure category as the stable snake_case token serde gives it (NULL on
/// success), so the stored value matches what the wire/UI use.
fn category_str(c: Option<ErrorCategory>) -> Option<String> {
    c.and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_string))
}

/// Record each attempt made for one candidate under `cycle_id`.
pub async fn record_attempts(
    pool: &SqlitePool,
    cycle_id: i64,
    candidate: &UpdateCandidate,
    outcomes: &[AttemptOutcome],
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    for o in outcomes {
        sqlx::query(
            "INSERT INTO update_attempts \
             (cycle_id, app_id, name, from_version, to_version, method, success, category, \
              exit_code, signature, sha256, detail, cost_usd, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(cycle_id)
        .bind(&candidate.id)
        .bind(&candidate.name)
        .bind(&candidate.current)
        .bind(o.installed_version.clone().unwrap_or_default())
        .bind(o.method.as_str())
        .bind(o.success as i64)
        .bind(category_str(o.category))
        .bind(o.exit_code)
        .bind(o.signature.clone().unwrap_or_default())
        .bind(o.sha256.clone().unwrap_or_default())
        .bind(&o.detail)
        .bind(o.cost_usd)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Latest attempt time (unix secs) per `app_id`, for fair staleness ordering. An app
/// tried most recently — including one that just FAILED — sorts last, so it can't
/// permanently starve never-tried apps when more updates than `max_apps_per_run` are
/// pending and the candidate collection order is otherwise fixed.
pub async fn last_attempt_times(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows =
        sqlx::query("SELECT app_id, MAX(created_at) AS last FROM update_attempts GROUP BY app_id")
            .fetch_all(pool)
            .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        let id: String = r.try_get("app_id")?;
        let last: String = r.try_get("last")?;
        let ts = chrono::DateTime::parse_from_rfc3339(&last)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        map.insert(id, ts);
    }
    Ok(map)
}

/// Latest AI-check time (unix secs) per `app_id`, so the unmanaged inventory can be
/// rotated stalest-first instead of always re-checking the same leading apps.
pub async fn last_check_times(pool: &SqlitePool) -> Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query("SELECT app_id, checked_at FROM update_checks")
        .fetch_all(pool)
        .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        let id: String = r.try_get("app_id")?;
        let at: String = r.try_get("checked_at")?;
        let ts = chrono::DateTime::parse_from_rfc3339(&at)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        map.insert(id, ts);
    }
    Ok(map)
}

/// Atomically record that every app in one valid AI response was checked now.
pub async fn record_checks(pool: &SqlitePool, app_ids: &[String]) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    for app_id in app_ids {
        anyhow::ensure!(
            crate::updater::config::valid_app_id(app_id),
            "invalid app id"
        );
        sqlx::query(
            "INSERT INTO update_checks (app_id, checked_at) VALUES (?, ?) \
             ON CONFLICT(app_id) DO UPDATE SET checked_at = excluded.checked_at",
        )
        .bind(app_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Persist the latest cycle that completed without a check/app failure.
async fn record_run_state(pool: &SqlitePool, key: &str, at: i64) -> Result<()> {
    anyhow::ensure!(at > 0, "update-run timestamp must be positive");
    sqlx::query(
        "INSERT INTO app_state (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(at.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn run_state(pool: &SqlitePool, key: &str) -> Result<i64> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM app_state WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    match value {
        Some(value) => Ok(value.parse()?),
        None => Ok(0),
    }
}

#[cfg(test)]
pub async fn record_run(pool: &SqlitePool, at: i64) -> Result<()> {
    record_run_state(pool, LAST_RUN_KEY, at).await
}

pub async fn last_run(pool: &SqlitePool) -> Result<i64> {
    run_state(pool, LAST_RUN_KEY).await
}

/// Atomically persist a cycle's scheduler timestamp and its UI evidence.
pub async fn record_cycle_status(
    pool: &SqlitePool,
    at: i64,
    notes: &[String],
    apps: &[eir_proto::UpdaterAppRow],
) -> Result<()> {
    anyhow::ensure!(at > 0, "update-run timestamp must be positive");
    let status = serde_json::to_string(&StoredCycleStatus {
        at,
        notes: notes.to_vec(),
        apps: apps.to_vec(),
    })?;
    let mut tx = pool.begin().await?;
    for (key, value) in [
        (LAST_RUN_KEY, at.to_string()),
        (LAST_CYCLE_STATUS_KEY, status),
    ] {
        sqlx::query(
            "INSERT INTO app_state (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn last_cycle_status(
    pool: &SqlitePool,
) -> Result<Option<(i64, Vec<String>, Vec<eir_proto::UpdaterAppRow>)>> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM app_state WHERE key = ?")
        .bind(LAST_CYCLE_STATUS_KEY)
        .fetch_optional(pool)
        .await?;
    value
        .map(|value| {
            let stored: StoredCycleStatus = serde_json::from_str(&value)?;
            anyhow::ensure!(stored.at > 0, "stored update-cycle timestamp is invalid");
            Ok((stored.at, stored.notes, stored.apps))
        })
        .transpose()
}

/// Persist the latest cycle that completed without a check/app failure.
pub async fn record_clean_run(pool: &SqlitePool, at: i64) -> Result<()> {
    record_run_state(pool, LAST_CLEAN_RUN_KEY, at).await
}

/// Read the latest persisted clean-cycle timestamp (0 when none has completed).
pub async fn last_clean_run(pool: &SqlitePool) -> Result<i64> {
    run_state(pool, LAST_CLEAN_RUN_KEY).await
}

/// Clear displayed cycle output while preserving scheduler and fair-ordering state.
pub async fn clear(pool: &SqlitePool) -> Result<()> {
    let last_run = last_run(pool).await?;
    let status = (last_run > 0)
        .then(|| {
            serde_json::to_string(&StoredCycleStatus {
                at: last_run,
                notes: vec![],
                apps: vec![],
            })
        })
        .transpose()?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO app_state (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(HISTORY_CLEARED_AT_KEY)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&mut *tx)
    .await?;
    if let Some(status) = status {
        sqlx::query(
            "INSERT INTO app_state (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(LAST_CYCLE_STATUS_KEY)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM app_state WHERE key = ?")
            .bind(LAST_CYCLE_STATUS_KEY)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The most recent attempts, newest first, for the UI's history view.
pub async fn recent(pool: &SqlitePool, limit: i64) -> Result<Vec<eir_proto::UpdateAttemptRow>> {
    let rows = sqlx::query(
        "SELECT name, method, success, detail, from_version, to_version, category, \
                 exit_code, created_at FROM update_attempts \
         WHERE julianday(created_at) > COALESCE( \
             (SELECT julianday(value) FROM app_state WHERE key = ?), 0) \
         ORDER BY id DESC LIMIT ?",
    )
    .bind(HISTORY_CLEARED_AT_KEY)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let created: String = r.try_get("created_at")?;
        let at = chrono::DateTime::parse_from_rfc3339(&created)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        out.push(eir_proto::UpdateAttemptRow {
            name: r.try_get("name")?,
            method: r.try_get("method")?,
            success: r.try_get::<i64, _>("success")? != 0,
            detail: r.try_get("detail").unwrap_or_default(),
            from_version: r.try_get("from_version").unwrap_or_default(),
            to_version: r.try_get("to_version").unwrap_or_default(),
            category: r.try_get("category").unwrap_or_default(),
            exit_code: r.try_get("exit_code").unwrap_or_default(),
            at,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::domain::{Method, Verification};

    #[tokio::test]
    async fn attempt_batch_rolls_back_when_any_row_fails() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("db");
        sqlx::query(
            "CREATE TABLE update_attempts (
                id INTEGER PRIMARY KEY,
                cycle_id INTEGER NOT NULL,
                app_id TEXT NOT NULL,
                name TEXT NOT NULL,
                from_version TEXT,
                to_version TEXT,
                method TEXT NOT NULL,
                success INTEGER NOT NULL,
                category TEXT,
                exit_code INTEGER,
                signature TEXT,
                sha256 TEXT,
                detail TEXT,
                cost_usd REAL,
                created_at TEXT NOT NULL
            );
            CREATE TRIGGER reject_native BEFORE INSERT ON update_attempts
            WHEN NEW.method = 'native'
            BEGIN SELECT RAISE(ABORT, 'write failed'); END;",
        )
        .execute(&pool)
        .await
        .expect("schema");
        let candidate = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1".into(),
            available: "2".into(),
            package_id: None,
            guidance: None,
            methods: vec![Method::Winget, Method::Native],
        };
        let outcomes = [
            AttemptOutcome::failed(Method::Winget, ErrorCategory::NotFound, "missing"),
            AttemptOutcome::failed(Method::Native, ErrorCategory::NotFound, "missing"),
        ];

        assert!(record_attempts(&pool, 1, &candidate, &outcomes)
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM update_attempts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migration_applies_and_attempt_round_trips() {
        // A temp-file DB so the pool's connections all see the same database.
        let path = std::env::temp_dir().join(format!("eir-hist-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!(
            "sqlite:{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.expect("open db");
        // Exercises the real migration set, including 0007.
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let cand = UpdateCandidate {
            id: "tool".into(),
            name: "Tool".into(),
            current: "1.0".into(),
            available: "2.0".into(),
            package_id: Some("Pub.Tool".into()),
            guidance: None,
            methods: vec![Method::Winget],
        };
        let ok = AttemptOutcome {
            method: Method::Winget,
            success: true,
            verification: Verification::Verified,
            category: None,
            exit_code: Some(0),
            installed_version: Some("2.0".into()),
            detail: "updated".into(),
            signature: None,
            sha256: None,
            cost_usd: 0.0,
        };
        let failed =
            AttemptOutcome::failed(Method::Native, ErrorCategory::SignatureRejected, "unsigned");
        record_attempts(&pool, 42, &cand, &[ok, failed])
            .await
            .expect("record");

        let rows = sqlx::query(
            "SELECT method, success, category, to_version FROM update_attempts \
             WHERE app_id = ? ORDER BY id",
        )
        .bind("tool")
        .fetch_all(&pool)
        .await
        .expect("query");
        assert_eq!(rows.len(), 2);

        let m0: String = rows[0].try_get("method").unwrap();
        let s0: i64 = rows[0].try_get("success").unwrap();
        let to0: String = rows[0].try_get("to_version").unwrap();
        assert_eq!(m0, "winget");
        assert_eq!(s0, 1);
        assert_eq!(to0, "2.0");

        let m1: String = rows[1].try_get("method").unwrap();
        let cat1: String = rows[1].try_get("category").unwrap();
        assert_eq!(m1, "native");
        assert_eq!(cat1, "signature_rejected");

        let recent_rows = recent(&pool, 10).await.expect("recent");
        assert_eq!(recent_rows[0].from_version, "1.0");
        assert_eq!(recent_rows[0].category, "signature_rejected");
        assert_eq!(recent_rows[1].to_version, "2.0");
        assert_eq!(recent_rows[1].exit_code, Some(0));

        assert_eq!(last_clean_run(&pool).await.expect("no clean run"), 0);
        assert_eq!(last_run(&pool).await.expect("no run"), 0);
        record_run(&pool, 1200).await.expect("record run");
        record_clean_run(&pool, 1234)
            .await
            .expect("record clean run");
        assert_eq!(last_run(&pool).await.expect("read run"), 1200);
        assert_eq!(last_clean_run(&pool).await.expect("read clean run"), 1234);
        let status_apps = vec![eir_proto::UpdaterAppRow {
            id: "update-check".into(),
            name: "Update check".into(),
            state: "failed".into(),
            ..Default::default()
        }];
        record_cycle_status(&pool, 1300, &["partial inventory".into()], &status_apps)
            .await
            .expect("record cycle status");
        let (at, notes, apps) = last_cycle_status(&pool)
            .await
            .expect("read cycle status")
            .expect("stored cycle status");
        assert_eq!(at, 1300);
        assert_eq!(notes, ["partial inventory"]);
        assert_eq!(apps[0].state, "failed");

        clear(&pool).await.expect("clear");
        assert!(recent(&pool, 10).await.expect("cleared recent").is_empty());
        let (_, notes, apps) = last_cycle_status(&pool)
            .await
            .expect("read cleared cycle status")
            .expect("cleared cycle status remains");
        assert!(notes.is_empty());
        assert!(apps.is_empty());
        assert!(last_attempt_times(&pool)
            .await
            .expect("preserved attempt ordering")
            .contains_key("tool"));
        assert_eq!(last_run(&pool).await.expect("preserved run"), 1300);
        assert_eq!(
            last_clean_run(&pool).await.expect("preserved clean run"),
            1234
        );

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}
