//! Durable user preferences for specific fix actions (`ignore` / `always_approve`).
//!
//! Keyed by [`crate::models::FixAction::dedup_key`] so regenerable AI parameters cannot
//! defeat the preference. Conservative defaults: an unknown preference is treated as
//! absent, and a missing table/row never blocks the decision loop.

use anyhow::{bail, Context, Result};
use sqlx::{Row, SqlitePool};

/// Hard cap so a misbehaving client cannot grow the preference table without bound.
pub const MAX_ACTION_PREFERENCES: usize = 256;
const MAX_SUMMARY_CHARS: usize = 500;
const MAX_TARGET_CHARS: usize = 500;
const MAX_KEY_CHARS: usize = 500;

/// Closed set of preferences a user may attach to a semantic action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    /// Do not queue this action for approval again.
    Ignore,
    /// Treat a future RequireApproval for this action as AutoApprove.
    AlwaysApprove,
}

impl Preference {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::AlwaysApprove => "always_approve",
        }
    }

    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "ignore" => Some(Self::Ignore),
            "always_approve" => Some(Self::AlwaysApprove),
            _ => None,
        }
    }
}

fn sanitize_text(raw: &str, max: usize) -> String {
    let trimmed: String = raw
        .chars()
        .filter(|c| *c != '\0' && *c != '\r')
        .take(max)
        .collect();
    trimmed.trim().to_string()
}

fn validate_key(action_key: &str) -> Result<String> {
    let key = sanitize_text(action_key, MAX_KEY_CHARS);
    if key.is_empty() {
        bail!("action preference key is empty");
    }
    Ok(key)
}

/// Insert or replace a preference for `action_key`. Enforces the table cap on insert of
/// a *new* key; updating an existing key always succeeds.
pub async fn set_preference(
    pool: &SqlitePool,
    action_key: &str,
    preference: Preference,
    summary: &str,
    target: &str,
) -> Result<()> {
    let key = validate_key(action_key)?;
    let summary = sanitize_text(summary, MAX_SUMMARY_CHARS);
    let target = sanitize_text(target, MAX_TARGET_CHARS);
    let summary = if summary.is_empty() {
        key.clone()
    } else {
        summary
    };
    let now = chrono::Utc::now().to_rfc3339();

    let existing: Option<String> =
        sqlx::query_scalar("SELECT preference FROM action_preferences WHERE action_key = ?")
            .bind(&key)
            .fetch_optional(pool)
            .await?;

    if existing.is_none() {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM action_preferences")
            .fetch_one(pool)
            .await?;
        if count >= MAX_ACTION_PREFERENCES as i64 {
            bail!("action preference limit reached ({MAX_ACTION_PREFERENCES})");
        }
    }

    sqlx::query(
        "INSERT INTO action_preferences (action_key, preference, summary, target, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(action_key) DO UPDATE SET
            preference = excluded.preference,
            summary = excluded.summary,
            target = excluded.target,
            created_at = excluded.created_at",
    )
    .bind(&key)
    .bind(preference.as_token())
    .bind(&summary)
    .bind(&target)
    .bind(&now)
    .execute(pool)
    .await
    .context("persisting action preference")?;
    Ok(())
}

/// Remove a preference. Returns true when a row was deleted.
pub async fn clear_preference(pool: &SqlitePool, action_key: &str) -> Result<bool> {
    let key = validate_key(action_key)?;
    let res = sqlx::query("DELETE FROM action_preferences WHERE action_key = ?")
        .bind(&key)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Look up the preference for one semantic action, if any.
pub async fn get_preference(pool: &SqlitePool, action_key: &str) -> Result<Option<Preference>> {
    let key = validate_key(action_key)?;
    let token: Option<String> =
        sqlx::query_scalar("SELECT preference FROM action_preferences WHERE action_key = ?")
            .bind(&key)
            .fetch_optional(pool)
            .await?;
    Ok(token.and_then(|t| Preference::from_token(&t)))
}

/// Every stored preference, newest first — for the Learned / Settings reverse UI.
pub async fn list_preferences(pool: &SqlitePool) -> Result<Vec<eir_proto::ActionPreferenceView>> {
    let rows = sqlx::query(
        "SELECT action_key, preference, summary, target, created_at
         FROM action_preferences
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(MAX_ACTION_PREFERENCES as i64)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let preference: String = r.try_get("preference")?;
        if Preference::from_token(&preference).is_none() {
            continue;
        }
        let created_at = r
            .try_get::<String, _>("created_at")
            .ok()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        out.push(eir_proto::ActionPreferenceView {
            action_key: r.try_get("action_key")?,
            preference,
            summary: r.try_get("summary").unwrap_or_default(),
            target: r.try_get("target").unwrap_or_default(),
            created_at,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!(
            "eir-prefs-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let url = format!(
            "sqlite:{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.expect("open");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        pool
    }

    #[tokio::test]
    async fn set_get_clear_round_trips() {
        let pool = pool().await;
        set_preference(
            &pool,
            "process_kill|chrome",
            Preference::Ignore,
            "Kill chrome.exe",
            "chrome",
        )
        .await
        .unwrap();
        assert_eq!(
            get_preference(&pool, "process_kill|chrome").await.unwrap(),
            Some(Preference::Ignore)
        );
        set_preference(
            &pool,
            "process_kill|chrome",
            Preference::AlwaysApprove,
            "Kill chrome.exe",
            "chrome",
        )
        .await
        .unwrap();
        assert_eq!(
            get_preference(&pool, "process_kill|chrome").await.unwrap(),
            Some(Preference::AlwaysApprove)
        );
        assert!(clear_preference(&pool, "process_kill|chrome")
            .await
            .unwrap());
        assert_eq!(
            get_preference(&pool, "process_kill|chrome").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn list_returns_newest_first_and_skips_unknown_tokens() {
        let pool = pool().await;
        set_preference(&pool, "a|1", Preference::Ignore, "A", "")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        set_preference(&pool, "b|2", Preference::AlwaysApprove, "B", "t")
            .await
            .unwrap();
        let list = list_preferences(&pool).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].action_key, "b|2");
        assert_eq!(list[0].preference, "always_approve");
        assert_eq!(list[1].preference, "ignore");
    }

    #[tokio::test]
    async fn empty_key_is_rejected() {
        let pool = pool().await;
        assert!(set_preference(&pool, "  ", Preference::Ignore, "x", "")
            .await
            .is_err());
    }
}
