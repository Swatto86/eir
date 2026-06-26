//! Persistence for learned facts: read the audit evidence the detectors need, record
//! approval rejections, and upsert/reconcile/query the `learned_facts` table (migrations
//! 0008/0009). Same bare-query/bind style as `audit.rs` and `updater::history`.

use super::detect::{AttemptRow, FeedbackRow, RejectionRow};
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;

const KIND_SELF_UPDATER: &str = "self_updater_suspected";

fn cutoff(window_days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(window_days)).to_rfc3339()
}

// ── Evidence reads ──────────────────────────────────────────────────────────────

/// Update attempts in the rolling window — evidence for self-updater and method detection.
pub async fn update_attempt_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<AttemptRow>> {
    let rows = sqlx::query(
        "SELECT app_id, cycle_id, success, exit_code, category, method, detail \
         FROM update_attempts WHERE created_at > ?",
    )
    .bind(cutoff(window_days))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(AttemptRow {
            app_id: r.try_get("app_id")?,
            cycle_id: r.try_get("cycle_id")?,
            success: r.try_get::<i64, _>("success")? != 0,
            exit_code: r.try_get::<Option<i64>, _>("exit_code")?.map(|v| v as i32),
            category: r.try_get::<Option<String>, _>("category")?,
            method: r.try_get("method").unwrap_or_default(),
            detail: r.try_get("detail").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Execution-feedback rows in the window — evidence for ineffective-fix detection.
pub async fn fix_feedback_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<FeedbackRow>> {
    let rows = sqlx::query(
        "SELECT action, succeeded, failed_services_before, failed_services_after \
         FROM execution_feedback WHERE recorded_at > ?",
    )
    .bind(cutoff(window_days))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(FeedbackRow {
            action: r.try_get("action")?,
            succeeded: r.try_get::<i64, _>("succeeded")? != 0,
            failed_services_before: r.try_get("failed_services_before")?,
            failed_services_after: r.try_get("failed_services_after")?,
        });
    }
    Ok(out)
}

/// Approval-rejection rows in the window — evidence for user-rejected-action detection.
pub async fn rejection_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<RejectionRow>> {
    let rows = sqlx::query("SELECT action_label FROM approval_rejections WHERE rejected_at > ?")
        .bind(cutoff(window_days))
        .fetch_all(pool)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(RejectionRow {
            action_label: r.try_get("action_label")?,
        });
    }
    Ok(out)
}

/// Record that the user rejected a queued action (drives RejectedSignal learning).
pub async fn record_rejection(
    pool: &SqlitePool,
    decision_id: i64,
    action_label: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO approval_rejections (decision_id, action_label, rejected_at) VALUES (?, ?, ?)",
    )
    .bind(decision_id)
    .bind(action_label)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Learned-fact writes ─────────────────────────────────────────────────────────

/// Insert or reinforce a learned fact. Idempotent via UNIQUE(kind,subject); a user
/// `pinned`/`disabled` override is preserved across reinforcement.
pub async fn upsert_fact(
    pool: &SqlitePool,
    kind: &str,
    subject: &str,
    effect_json: &str,
    evidence: &str,
    window_days: i64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO learned_facts \
            (kind, subject, effect_json, evidence_count, evidence_json, window_days, \
             first_seen_at, last_reinforced_at, status, source) \
         VALUES (?, ?, ?, 0, ?, ?, ?, ?, 'active', 'detector') \
         ON CONFLICT(kind, subject) DO UPDATE SET \
            effect_json = excluded.effect_json, \
            evidence_json = excluded.evidence_json, \
            last_reinforced_at = excluded.last_reinforced_at, \
            status = CASE WHEN learned_facts.status IN ('user_disabled', 'user_pinned') \
                          THEN learned_facts.status ELSE 'active' END",
    )
    .bind(kind)
    .bind(subject)
    .bind(effect_json)
    .bind(evidence)
    .bind(window_days)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark as `expired` any `active` detector fact of this kind whose subject is no longer
/// supported by current evidence (`current`). This is the re-probe/decay: once a skip or
/// deprioritisation stops being re-confirmed (its evidence ages out of the window) it
/// lapses, so the behaviour self-corrects. User overrides are never touched.
pub async fn expire_unsupported(
    pool: &SqlitePool,
    kind: &str,
    current: &HashSet<String>,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT subject FROM learned_facts \
         WHERE kind = ? AND source = 'detector' AND status = 'active'",
    )
    .bind(kind)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let subject: String = r.try_get("subject")?;
        if !current.contains(&subject) {
            sqlx::query(
                "UPDATE learned_facts SET status = 'expired' \
                 WHERE kind = ? AND subject = ? AND source = 'detector' AND status = 'active'",
            )
            .bind(kind)
            .bind(&subject)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Remove detector-derived learned facts (the user's "Clear" reset). Only `active` and
/// `expired` rows go; `pinned`/`disabled` overrides are preserved. Returns rows removed.
pub async fn clear_detector_facts(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM learned_facts WHERE source = 'detector' AND status IN ('active', 'expired')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ── Learned-fact reads ──────────────────────────────────────────────────────────

/// A learned fact in force, for application at the decision seams.
pub struct FactRow {
    pub kind: String,
    pub subject: String,
    pub effect_json: String,
    pub evidence: String,
}

/// All facts currently IN FORCE (`active` or `user_pinned`). Consulted by `apply`.
pub async fn active_facts(pool: &SqlitePool) -> Result<Vec<FactRow>> {
    let rows = sqlx::query(
        "SELECT kind, subject, effect_json, evidence_json FROM learned_facts \
         WHERE status IN ('active', 'user_pinned')",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(FactRow {
            kind: r.try_get("kind")?,
            subject: r.try_get("subject")?,
            effect_json: r.try_get("effect_json")?,
            evidence: r.try_get("evidence_json").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Build the plain-English summary for the UI card from a fact's kind + subject.
fn view_summary(kind: &str, subject: &str) -> String {
    match super::LearnedFactKind::from_token(kind) {
        Some(super::LearnedFactKind::SelfUpdaterSuspected) => {
            format!("Not managing updates for '{subject}' — it self-updates / keeps timing out")
        }
        Some(super::LearnedFactKind::MethodFailing) => {
            let (app, method) = subject
                .split_once(super::SUBJECT_SEP)
                .unwrap_or((subject, "?"));
            format!("Updating '{app}': trying '{method}' last (it keeps failing here)")
        }
        Some(super::LearnedFactKind::FixIneffective) => {
            format!("Lower confidence in '{subject}' fixes — they never resolve the fault here")
        }
        Some(super::LearnedFactKind::RejectedSignal) => {
            format!("Proposing '{subject}' less readily — you keep rejecting it")
        }
        None => format!("{kind}: {subject}"),
    }
}

/// Every learned fact (all statuses), newest first, for the UI transparency card. A
/// Tier-2 AI explanation, when present, replaces the deterministic summary.
pub async fn facts_for_view(pool: &SqlitePool) -> Result<Vec<eir_proto::LearnedFactView>> {
    let rows = sqlx::query(
        "SELECT id, kind, subject, evidence_json, status, source, ai_explanation FROM learned_facts \
         ORDER BY (status = 'user_disabled'), (status = 'expired'), last_reinforced_at DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let kind: String = r.try_get("kind")?;
        let subject: String = r.try_get("subject")?;
        let ai: Option<String> = r.try_get("ai_explanation")?;
        let labelled = ai.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        let summary = if labelled {
            ai.unwrap()
        } else {
            view_summary(&kind, &subject)
        };
        // Provenance shown to the user is "ai_labelled" only when an explanation exists;
        // the DB source column stays 'detector' to preserve the decay/clear lifecycle.
        let source = if labelled {
            "ai_labelled".to_string()
        } else {
            r.try_get("source")?
        };
        out.push(eir_proto::LearnedFactView {
            id: r.try_get("id")?,
            summary,
            detail: r.try_get("evidence_json").unwrap_or_default(),
            status: r.try_get("status")?,
            source,
        });
    }
    Ok(out)
}

/// One detector fact the Tier-2 labeller has NOT yet attempted (active/pinned only).
/// Returns (id, kind, subject, evidence). None when every fact has been attempted —
/// keying on the attempt marker (not on ai_explanation) is what bounds total AI calls to
/// one per fact, even when a fact's explanation can't be produced.
pub async fn next_unlabelled(pool: &SqlitePool) -> Result<Option<(i64, String, String, String)>> {
    let row = sqlx::query(
        "SELECT id, kind, subject, evidence_json FROM learned_facts \
         WHERE source = 'detector' AND status IN ('active', 'user_pinned') \
           AND ai_label_attempted_at IS NULL \
         ORDER BY last_reinforced_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("id")?,
            r.try_get("kind")?,
            r.try_get("subject")?,
            r.try_get("evidence_json").unwrap_or_default(),
        ))),
        None => Ok(None),
    }
}

/// Mark that the labeller has attempted this fact, so it is not re-queried regardless of
/// whether a usable explanation was produced (a re-attempt only happens if the fact is
/// forgotten and re-forms as a new row).
pub async fn mark_label_attempted(pool: &SqlitePool, id: i64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE learned_facts SET ai_label_attempted_at = ? WHERE id = ?")
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Attach a Tier-2 AI explanation to a fact. Text only — the AI never changes a fact's
/// kind, subject, or effect, so this cannot alter behaviour. The `source` column stays
/// `'detector'` so the fact keeps its detector lifecycle (decay via `expire_unsupported`,
/// reset via `clear_detector_facts`); the UI's "AI" provenance is derived from the
/// presence of the explanation, not from this column.
pub async fn set_ai_explanation(pool: &SqlitePool, id: i64, explanation: &str) -> Result<()> {
    sqlx::query("UPDATE learned_facts SET ai_explanation = ? WHERE id = ?")
        .bind(explanation)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Apply a user override to a learned fact by id: "pin" (always keep), "disable"
/// (always ignore), or "forget" (delete the row).
pub async fn set_learned_fact(pool: &SqlitePool, id: i64, op: &str) -> Result<()> {
    match op {
        "forget" => {
            sqlx::query("DELETE FROM learned_facts WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
        }
        "pin" | "disable" => {
            let status = if op == "pin" {
                "user_pinned"
            } else {
                "user_disabled"
            };
            sqlx::query("UPDATE learned_facts SET status = ? WHERE id = ?")
                .bind(status)
                .bind(id)
                .execute(pool)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

/// Just the self-updater subjects in force — the updater's candidate filter consults this.
pub async fn active_self_updater_subjects(pool: &SqlitePool) -> Result<HashSet<String>> {
    let rows = sqlx::query(
        "SELECT subject FROM learned_facts \
         WHERE kind = ? AND status IN ('active', 'user_pinned')",
    )
    .bind(KIND_SELF_UPDATER)
    .fetch_all(pool)
    .await?;
    let mut set = HashSet::with_capacity(rows.len());
    for r in rows {
        set.insert(r.try_get::<String, _>("subject")?);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{Effect, LearnedFactKind};

    async fn test_pool(tag: &str) -> SqlitePool {
        let path = std::env::temp_dir().join(format!("eir-learn-{tag}-{}.db", std::process::id()));
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
        pool
    }

    #[tokio::test]
    async fn fact_round_trips_reinforces_and_reconciles() {
        let pool = test_pool("round").await;
        assert!(active_self_updater_subjects(&pool)
            .await
            .unwrap()
            .is_empty());

        let su = LearnedFactKind::SelfUpdaterSuspected.as_token();
        upsert_fact(
            &pool,
            su,
            "discord",
            &Effect::Skip.to_json(),
            "3 timeouts",
            30,
        )
        .await
        .unwrap();
        assert!(active_self_updater_subjects(&pool)
            .await
            .unwrap()
            .contains("discord"));

        // Reinforce: no duplicate row.
        upsert_fact(
            &pool,
            su,
            "discord",
            &Effect::Skip.to_json(),
            "5 timeouts",
            30,
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM learned_facts")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(n, 1);

        // Reconcile with empty current → expires; user override survives.
        sqlx::query(
            "INSERT INTO learned_facts (kind, subject, effect_json, first_seen_at, last_reinforced_at, status, source) \
             VALUES (?, 'pinned', '{\"type\":\"skip\"}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'user_pinned', 'detector')",
        )
        .bind(su)
        .execute(&pool)
        .await
        .unwrap();
        expire_unsupported(&pool, su, &HashSet::new())
            .await
            .unwrap();
        let active = active_self_updater_subjects(&pool).await.unwrap();
        assert!(
            !active.contains("discord"),
            "expired detector fact must lapse"
        );
        assert!(active.contains("pinned"), "user_pinned survives reconcile");

        // Clear removes detector facts but preserves the user override.
        assert_eq!(clear_detector_facts(&pool).await.unwrap(), 1);
        let after = active_facts(&pool).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].subject, "pinned");
        drop(pool);
    }

    #[tokio::test]
    async fn rejection_round_trips() {
        let pool = test_pool("reject").await;
        record_rejection(&pool, 1, "ProcessKill { process_name: \"x\" }")
            .await
            .unwrap();
        let rows = rejection_rows(&pool, 30).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_label, "ProcessKill { process_name: \"x\" }");
        drop(pool);
    }

    #[tokio::test]
    async fn labelling_is_attempted_at_most_once_per_fact() {
        let pool = test_pool("label").await;
        let su = LearnedFactKind::SelfUpdaterSuspected.as_token();
        upsert_fact(
            &pool,
            su,
            "discord",
            &Effect::Skip.to_json(),
            "3 timeouts",
            30,
        )
        .await
        .unwrap();

        // Before any attempt, the fact is offered to the labeller.
        let first = next_unlabelled(&pool).await.unwrap();
        assert_eq!(first.as_ref().map(|f| f.2.as_str()), Some("discord"));
        let id = first.unwrap().0;

        // After marking the attempt — even with NO explanation stored (the unusable-reply
        // case) — it must NOT be offered again, so it can't re-fire an AI call each cycle.
        mark_label_attempted(&pool, id).await.unwrap();
        assert!(next_unlabelled(&pool).await.unwrap().is_none());
        drop(pool);
    }
}
