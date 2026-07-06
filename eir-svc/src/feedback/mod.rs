use crate::models::{FixAction, SystemState};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

/// How long after a fix executes before its "after" state is measured. A fix (a service
/// restart especially) needs a moment to settle; measuring on the very next tick can read a
/// still-transitioning state. Rows younger than this stay pending and are measured a later
/// cycle — so effectiveness reflects the settled outcome, not a same-tick snapshot.
const SETTLE_SECS: i64 = 120;

/// The RFC3339 cutoff for `now`: a feedback row is past its settle window iff its `recorded_at`
/// is at or before this. `Utc::now().to_rfc3339()` emits fixed-width `+00:00` strings, so a
/// lexical `<=` on two such strings matches chronological order — the same idiom `prune_old`
/// relies on. Pure so the settle boundary is unit-testable without a DB.
fn settle_cutoff(now: DateTime<Utc>) -> String {
    (now - chrono::Duration::seconds(SETTLE_SECS)).to_rfc3339()
}

/// The fault indicator a later `SystemState` can check for a fix — the service name for
/// service actions, `""` for actions with no metric-checkable target. Lets effectiveness be
/// attributed to the *specific* fault a fix addressed, instead of a global metric that an
/// unrelated concurrent fault could move.
pub fn action_target(action: &FixAction) -> String {
    match action {
        FixAction::ServiceRestart { service_name }
        | FixAction::ServiceStart { service_name }
        | FixAction::ServiceStop { service_name } => service_name.clone(),
        _ => String::new(),
    }
}

/// Record the "before" state at execution time, plus the fix's `target`. A later cycle
/// (past the settle window) fills in "after" and whether the target cleared.
pub async fn record(
    pool: &SqlitePool,
    execution_log_id: i64,
    action: &str,
    succeeded: bool,
    target: &str,
    state: &SystemState,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO execution_feedback
         (execution_log_id, action, succeeded, cpu_before, memory_before,
          failed_services_before, disk_before, target, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(execution_log_id)
    .bind(action)
    .bind(if succeeded { 1i64 } else { 0i64 })
    .bind(state.cpu_usage_percent as f64)
    .bind(state.memory_usage_percent as f64)
    .bind(state.failed_services.len() as i64)
    .bind(state.disk_usage_percent as f64)
    .bind(target)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fill in "after" metrics for feedback rows that have passed the settle window, and record
/// whether each fix's *targeted* fault actually cleared. Call once per decision cycle after
/// signals are collected.
pub async fn update_after_states(pool: &SqlitePool, state: &SystemState) -> Result<()> {
    // Only rows recorded at least SETTLE_SECS ago (see the const). Younger rows stay pending
    // and are measured a later cycle. No LIMIT — executions per cycle are few, so the
    // eligible set is small; a LIMIT would strand older rows at NULL forever.
    let cutoff = settle_cutoff(Utc::now());
    let rows = sqlx::query(
        "SELECT id, cpu_before, memory_before, failed_services_before, disk_before, target
         FROM execution_feedback
         WHERE cpu_after IS NULL AND recorded_at <= ?
         ORDER BY id DESC",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;

    let cpu_after = state.cpu_usage_percent as f64;
    let mem_after = state.memory_usage_percent as f64;
    let fs_after = state.failed_services.len() as i64;
    let disk_after = state.disk_usage_percent as f64;

    for row in rows {
        let id: i64 = row.try_get("id")?;
        let cpu_before: Option<f64> = row.try_get("cpu_before")?;
        let mem_before: Option<f64> = row.try_get("memory_before")?;
        let fs_before: Option<i64> = row.try_get("failed_services_before")?;
        let disk_before: Option<f64> = row.try_get("disk_before")?;
        let target: String = row.try_get("target")?;
        let score = improvement_score(
            cpu_before,
            mem_before,
            fs_before,
            disk_before,
            cpu_after,
            mem_after,
            fs_after,
            disk_after,
        );
        // Targeted outcome: for a service-target fix, did *that* service clear? `None` (no
        // target) means "not metric-checkable" — judged only by the resource score downstream,
        // never branded ineffective.
        // Windows service names are case-insensitive, and the AI-supplied name may differ in
        // case from the enumerator's — compare case-insensitively so a cleared service isn't
        // wrongly recorded as unresolved.
        let resolved: Option<i64> = if target.is_empty() {
            None
        } else if state
            .failed_services
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&target))
        {
            Some(0) // target still failed → not resolved
        } else {
            Some(1) // target cleared
        };

        sqlx::query(
            "UPDATE execution_feedback
             SET cpu_after = ?, memory_after = ?, failed_services_after = ?,
                 disk_after = ?, improvement_score = ?, resolved = ?
             WHERE id = ?",
        )
        .bind(cpu_after)
        .bind(mem_after)
        .bind(fs_after)
        .bind(disk_after)
        .bind(score)
        .bind(resolved)
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

#[allow(clippy::too_many_arguments)]
fn improvement_score(
    cpu_before: Option<f64>,
    mem_before: Option<f64>,
    fs_before: Option<i64>,
    disk_before: Option<f64>,
    cpu_after: f64,
    mem_after: f64,
    fs_after: i64,
    disk_after: f64,
) -> f64 {
    // Positive score = system improved; negative = degraded. A None "before" is a 0 delta.
    let cpu_delta = cpu_before.map(|b| b - cpu_after).unwrap_or(0.0); // CPU drop is good
    let mem_delta = mem_before.map(|b| b - mem_after).unwrap_or(0.0); // memory drop is good
    let disk_delta = disk_before.map(|b| b - disk_after).unwrap_or(0.0); // disk-used drop is good
    let fs_delta = fs_before.map(|b| (b - fs_after) as f64).unwrap_or(0.0); // fewer failed services is good

    cpu_delta * 0.3 + mem_delta * 0.3 + disk_delta * 0.3 + fs_delta * 10.0
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
        "SELECT f.action, f.succeeded, f.improvement_score, f.resolved, f.recorded_at, e.output
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
        let resolved: Option<i64> = row.try_get("resolved")?;
        let ts: String = row.try_get("recorded_at")?;
        let output: Option<String> = row.try_get("output")?;
        let short_ts = &ts[..ts.len().min(16)];

        let succeeded = succeeded != 0;
        let outcome = if succeeded { "SUCCESS" } else { "FAILURE" };
        // Prefer the targeted outcome (did the specific fault clear?) — the strongest
        // effectiveness signal — and fall back to the resource-delta score when the fix has
        // no metric-checkable target.
        let delta_str = match resolved {
            Some(1) => ", and it CLEARED the targeted fault".to_string(),
            Some(_) => ", but the targeted fault did NOT clear".to_string(),
            None => match improvement {
                Some(s) if s > 1.0 => format!(", improved (+{s:.1})"),
                Some(s) if s < -1.0 => format!(", degraded ({s:.1})"),
                Some(_) => ", no measurable change".to_string(),
                None => " (pending measurement)".to_string(),
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_target_is_the_service_name_for_service_actions_only() {
        assert_eq!(
            action_target(&FixAction::ServiceRestart {
                service_name: "Spooler".into()
            }),
            "Spooler"
        );
        assert_eq!(
            action_target(&FixAction::ServiceStop {
                service_name: "W32Time".into()
            }),
            "W32Time"
        );
        // Non-service actions have no metric-checkable target.
        assert_eq!(
            action_target(&FixAction::DiskCleanup {
                target: "temp".into()
            }),
            ""
        );
        assert_eq!(
            action_target(&FixAction::ProcessKill {
                process_name: "x".into()
            }),
            ""
        );
    }

    #[test]
    fn settle_gate_excludes_young_rows_and_admits_older_ones() {
        // The SQL filter is `recorded_at <= settle_cutoff(now)`, relying on lexical order of
        // RFC3339 strings matching chronological order. Assert that against the real cutoff.
        let now = DateTime::parse_from_rfc3339("2026-07-06T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let cutoff = settle_cutoff(now);
        let settled = |ago: i64| (now - chrono::Duration::seconds(ago)).to_rfc3339() <= cutoff;
        // A fix recorded 60s ago hasn't settled (SETTLE_SECS = 120); one at 180s ago has.
        assert!(!settled(60), "60s < 120s window → not settled");
        assert!(settled(180), "180s ≥ 120s window → settled");
        // Exactly at the boundary counts as settled (the SQL `<=` is inclusive).
        assert!(settled(SETTLE_SECS), "boundary is inclusive");
    }

    #[test]
    fn improvement_score_counts_disk_and_treats_none_as_zero_delta() {
        // A disk-used drop is now a positive contribution (previously ignored).
        let with_disk = improvement_score(None, None, None, Some(90.0), 0.0, 0.0, 0, 80.0); // disk 90→80
        assert!(
            with_disk > 0.0,
            "disk drop should raise the score: {with_disk}"
        );
        // All None befores → 0 (no panic, no spurious score).
        assert_eq!(
            improvement_score(None, None, None, None, 5.0, 5.0, 0, 5.0),
            0.0
        );
        // A cleared failed service dominates (weight 10).
        let fs = improvement_score(None, None, Some(1), None, 0.0, 0.0, 0, 0.0);
        assert!((fs - 10.0).abs() < 1e-6);
    }
}
