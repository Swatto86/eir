//! Persist every update attempt to the audit DB (the `update_attempts` table from
//! migration 0007). Follows the same bare-query/bind pattern as `audit.rs`. This is
//! the audit trail for unattended installs and the data the UI's history view reads.

use crate::updater::domain::{AttemptOutcome, ErrorCategory, UpdateCandidate};
use anyhow::Result;
use sqlx::{Row, SqlitePool};

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
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The most recent attempts, newest first, for the UI's history view.
pub async fn recent(pool: &SqlitePool, limit: i64) -> Result<Vec<eir_proto::UpdateAttemptRow>> {
    let rows = sqlx::query(
        "SELECT name, method, success, detail, created_at FROM update_attempts \
         ORDER BY id DESC LIMIT ?",
    )
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

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}
