use crate::models::{
    CallUsage, ClaudeDecision, ExecutionResult, FixAction, PastDecision, PendingApproval,
    RegistryUndo, SignalSnapshot, SystemState,
};
use anyhow::Result;
use chrono::Utc;
use eir_proto::{ApprovalInfo, UsageSummary};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

/// Set a value in the small `app_state` key/value store (upsert). For transient state
/// that must survive a restart — currently Game Mode's power-restore GUID.
pub async fn set_state(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO app_state (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read a value from `app_state`, or `None` if the key is absent.
pub async fn get_state(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM app_state WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        Some(r) => Some(r.try_get("value")?),
        None => None,
    })
}

/// Delete a key from `app_state` (idempotent).
pub async fn delete_state(pool: &SqlitePool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM app_state WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn init_db(path: &str) -> Result<SqlitePool> {
    // WAL lets the concurrent writers (decision loop, executor worker, update-cycle
    // task, labeller) proceed with a reader without serialising on a rollback
    // journal; a generous busy_timeout absorbs the remaining writer contention so a
    // burst doesn't drop audit rows (which would break the rate-limit circuit
    // breaker and NULL effectiveness feedback).
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{path}?mode=rwc"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(15));
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

/// Insert a minimal `decisions` row for a user-initiated action (a disk cleanup or
/// startup toggle) so its execution/approval has a real `decision_id` and it shows up
/// in decision history. Writes no `system_state_history` row, so it never pollutes the
/// resource timeline or the trend detector.
pub async fn log_manual_decision(pool: &SqlitePool, summary: &str) -> Result<i64> {
    let timestamp = Utc::now().to_rfc3339();
    let response = serde_json::json!({
        "analysis": summary,
        "problems": [],
        "needs_deeper_analysis": false,
    })
    .to_string();
    let id = sqlx::query(
        "INSERT INTO decisions (timestamp, signal_snapshot, claude_response, confidence, executed)
         VALUES (?, '{}', ?, 1.0, 0)",
    )
    .bind(&timestamp)
    .bind(&response)
    .execute(pool)
    .await?
    .last_insert_rowid();
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
                needs_deeper_analysis: false,
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

/// Load the advisor's persisted escalation spend + count for `day` (YYYY-MM-DD UTC).
/// Returns (0.0, 0) when there is no row yet for that day.
pub async fn load_advisor_day(pool: &SqlitePool, day: &str) -> Result<(f64, u32)> {
    let row = sqlx::query("SELECT spent_usd, escalations FROM advisor_daily_spend WHERE day = ?")
        .bind(day)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => {
            let spent: f64 = r.try_get(0)?;
            let escalations: i64 = r.try_get(1)?;
            Ok((spent, escalations.max(0) as u32))
        }
        None => Ok((0.0, 0)),
    }
}

/// Persist the advisor's escalation spend + count for `day` (upsert), so the daily
/// budget / escalation caps survive a service restart.
pub async fn save_advisor_day(
    pool: &SqlitePool,
    day: &str,
    spent_usd: f64,
    escalations: u32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO advisor_daily_spend (day, spent_usd, escalations)
         VALUES (?, ?, ?)
         ON CONFLICT(day) DO UPDATE SET spent_usd = excluded.spent_usd,
                                        escalations = excluded.escalations",
    )
    .bind(day)
    .bind(spent_usd)
    .bind(escalations as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// One row of the per-cycle resource history used for trend detection.
pub struct MetricSample {
    pub cpu: f64,
    pub mem: f64,
    pub disk: f64,
}

/// Read the last `limit` resource samples (chronological order) from the
/// `system_state_history` — a rich per-cycle series that was previously written but
/// never read.
pub async fn recent_metrics(pool: &SqlitePool, limit: i64) -> Result<Vec<MetricSample>> {
    let rows = sqlx::query(
        "SELECT cpu_usage, memory_usage, disk_usage
         FROM system_state_history ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // Query is newest-first; reverse to chronological so trends read left→right.
    Ok(rows
        .iter()
        .rev()
        .map(|r| MetricSample {
            cpu: r.try_get::<Option<f64>, _>(0).ok().flatten().unwrap_or(0.0),
            mem: r.try_get::<Option<f64>, _>(1).ok().flatten().unwrap_or(0.0),
            disk: r.try_get::<Option<f64>, _>(2).ok().flatten().unwrap_or(0.0),
        })
        .collect())
}

/// A one-line resource-trend note for the AI prompt, or `None` when nothing is worth
/// flagging. Reads the recent history and compares the first vs second half of the
/// window — a slow climb (disk filling, sustained CPU/memory rise) that a single
/// snapshot can't show. Thresholds are heuristic and deliberately conservative to
/// avoid noise on a healthy machine.
pub async fn metric_trend(pool: &SqlitePool) -> Result<Option<String>> {
    let samples = recent_metrics(pool, 12).await?;
    Ok(summarise_trend(&samples))
}

/// Pure trend summariser (unit-tested). Needs at least 6 samples; compares the mean of
/// the older half to the newer half and flags a metric only when it is both rising and
/// already elevated.
fn summarise_trend(samples: &[MetricSample]) -> Option<String> {
    if samples.len() < 6 {
        return None;
    }
    let half = |get: fn(&MetricSample) -> f64| -> (f64, f64) {
        let mid = samples.len() / 2;
        let first: f64 = samples[..mid].iter().map(get).sum::<f64>() / mid as f64;
        let last: f64 = samples[mid..].iter().map(get).sum::<f64>() / (samples.len() - mid) as f64;
        (first, last)
    };
    let mut notes = Vec::new();
    let (df, dl) = half(|s| s.disk);
    if dl - df >= 3.0 && dl >= 75.0 {
        notes.push(format!("disk usage trending up ({df:.0}% → {dl:.0}%)"));
    }
    let (cf, cl) = half(|s| s.cpu);
    if cl - cf >= 15.0 && cl >= 70.0 {
        notes.push(format!("CPU load trending up ({cf:.0}% → {cl:.0}%)"));
    }
    let (mf, ml) = half(|s| s.mem);
    if ml - mf >= 15.0 && ml >= 80.0 {
        notes.push(format!("memory usage trending up ({mf:.0}% → {ml:.0}%)"));
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!(
            "RESOURCE TREND (last {} cycles — a slow climb a single snapshot can't show): {}. \
             Treat as a fault only if it is heading toward exhaustion (disk under ~10% free, OOM); \
             otherwise note it, don't act.",
            samples.len(),
            notes.join("; ")
        ))
    }
}

/// Read the resource history since `cutoff_rfc3339` (chronological order), thinned to
/// at most `cap` points so the dashboard-timeline payload stays small. Reads the same
/// `system_state_history` series the trend detector uses (rows are written once per
/// analysed cycle).
pub async fn metric_history(
    pool: &SqlitePool,
    cutoff_rfc3339: &str,
    cap: usize,
) -> Result<Vec<eir_proto::MetricPoint>> {
    let rows = sqlx::query(
        "SELECT timestamp, cpu_usage, memory_usage, disk_usage
         FROM system_state_history WHERE timestamp >= ? ORDER BY id ASC",
    )
    .bind(cutoff_rfc3339)
    .fetch_all(pool)
    .await?;
    let points: Vec<eir_proto::MetricPoint> = rows
        .iter()
        .map(|r| {
            let ts: String = r.try_get::<String, _>(0).unwrap_or_default();
            let at = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.timestamp())
                .unwrap_or(0);
            eir_proto::MetricPoint {
                at,
                cpu: r.try_get::<Option<f64>, _>(1).ok().flatten().unwrap_or(0.0) as f32,
                memory: r.try_get::<Option<f64>, _>(2).ok().flatten().unwrap_or(0.0) as f32,
                disk: r.try_get::<Option<f64>, _>(3).ok().flatten().unwrap_or(0.0) as f32,
            }
        })
        .collect();
    Ok(thin_points(points, cap))
}

/// Downsample `points` to at most `cap` by uniform stride, always keeping the first
/// and last. Pure, unit-tested.
fn thin_points(points: Vec<eir_proto::MetricPoint>, cap: usize) -> Vec<eir_proto::MetricPoint> {
    let n = points.len();
    if cap < 2 || n <= cap {
        return points;
    }
    (0..cap)
        .map(|i| points[i * (n - 1) / (cap - 1)].clone())
        .collect()
}

/// Aggregated audit counts over a window, for the weekly health digest.
#[derive(Debug, Default, Clone)]
pub struct DigestStats {
    pub decisions: i64,
    pub exec_total: i64,
    pub exec_success: i64,
    pub updates_total: i64,
    pub updates_success: i64,
    pub learned_facts: i64,
    pub spend_usd: f64,
}

/// Roll up the audit tables since `cutoff_rfc3339` (a UTC RFC3339 string; timestamps
/// are stored in the same format so a lexical `>=` compares chronologically). Feeds the
/// digest generator — summary counts only, never raw snapshots.
pub async fn digest_stats(pool: &SqlitePool, cutoff_rfc3339: &str) -> Result<DigestStats> {
    let mut s = DigestStats::default();

    let d: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM decisions WHERE timestamp >= ?")
        .bind(cutoff_rfc3339)
        .fetch_one(pool)
        .await?;
    s.decisions = d.0;

    let e: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(success), 0) FROM execution_log WHERE executed_at >= ?",
    )
    .bind(cutoff_rfc3339)
    .fetch_one(pool)
    .await?;
    s.exec_total = e.0;
    s.exec_success = e.1;

    let u: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(success), 0) FROM update_attempts WHERE created_at >= ?",
    )
    .bind(cutoff_rfc3339)
    .fetch_one(pool)
    .await?;
    s.updates_total = u.0;
    s.updates_success = u.1;

    let c: (f64,) =
        sqlx::query_as("SELECT COALESCE(SUM(cost_usd), 0) FROM usage_log WHERE timestamp >= ?")
            .bind(cutoff_rfc3339)
            .fetch_one(pool)
            .await?;
    s.spend_usd = c.0;

    let f: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM learned_facts")
        .fetch_one(pool)
        .await?;
    s.learned_facts = f.0;

    Ok(s)
}

/// Persist a generated health digest.
pub async fn save_digest(pool: &SqlitePool, text: &str) -> Result<()> {
    sqlx::query("INSERT INTO health_digest (text, generated_at) VALUES (?, ?)")
        .bind(text)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

/// The most recent health digest as (text, unix-seconds), or `None` if none exists.
pub async fn latest_digest(pool: &SqlitePool) -> Result<Option<(String, i64)>> {
    let row = sqlx::query("SELECT text, generated_at FROM health_digest ORDER BY id DESC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => {
            let text: String = r.try_get(0)?;
            let ts: String = r.try_get(1)?;
            let at = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.timestamp())
                .unwrap_or(0);
            Ok(Some((text, at)))
        }
        None => Ok(None),
    }
}

/// Persist a registry-undo snapshot linked to its execution; returns the row id the
/// UI uses to trigger a one-click revert.
pub async fn save_registry_undo(
    pool: &SqlitePool,
    execution_id: i64,
    undo: &RegistryUndo,
) -> Result<i64> {
    let id = sqlx::query(
        "INSERT INTO registry_undo
           (execution_id, key_path, value_name, prior_existed, prior_data, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(execution_id)
    .bind(&undo.key_path)
    .bind(&undo.value_name)
    .bind(undo.prior_existed as i64)
    .bind(&undo.prior_data)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

/// Atomically CLAIM a not-yet-undone registry-undo record for reverting: flip
/// `undone` 0→1 and, only if this call is the one that flipped it, return the
/// snapshot. Two concurrent undo requests for the same id can't both win (SQLite
/// serialises writers), so this closes the check-then-act double-undo race. Returns
/// `None` if the record doesn't exist or was already claimed/reverted. On a restore
/// failure the caller should [`unclaim_registry_undo`] so the user can retry.
pub async fn claim_registry_undo(pool: &SqlitePool, id: i64) -> Result<Option<RegistryUndo>> {
    let flipped = sqlx::query("UPDATE registry_undo SET undone = 1 WHERE id = ? AND undone = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if flipped == 0 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT key_path, value_name, prior_existed, prior_data FROM registry_undo WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(RegistryUndo {
            key_path: r.try_get(0)?,
            value_name: r.try_get(1)?,
            prior_existed: r.try_get::<i64, _>(2)? != 0,
            prior_data: r.try_get(3)?,
        })),
        None => Ok(None),
    }
}

/// Release a claim made by [`claim_registry_undo`] (restore failed) so a later attempt
/// can retry it.
pub async fn unclaim_registry_undo(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE registry_undo SET undone = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
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

// ── Pending approvals ─────────────────────────────────────────────────────────

/// Persist a fix awaiting the user's decision and return its id (the row id,
/// which is also the approval id the UI uses). Survives a service restart so the
/// user can still act on it afterwards.
pub async fn insert_pending_approval(
    pool: &SqlitePool,
    decision_id: i64,
    action: &FixAction,
    info: &ApprovalInfo,
    baseline: &SystemState,
) -> Result<i64> {
    let created_at = Utc::now().to_rfc3339();
    let action_json = serde_json::to_string(action)?;
    let info_json = serde_json::to_string(info)?;
    let baseline_json = serde_json::to_string(baseline)?;
    let id = sqlx::query(
        "INSERT INTO pending_approvals
         (created_at, decision_id, action_json, info_json, baseline_json)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&created_at)
    .bind(decision_id)
    .bind(&action_json)
    .bind(&info_json)
    .bind(&baseline_json)
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

/// Load all outstanding approvals (oldest first), reconstructing the action and
/// feedback baseline. Rows whose stored JSON no longer deserializes — e.g. an
/// action shape removed in an upgrade — are dropped (and deleted) rather than
/// failing the whole load.
pub async fn load_pending_approvals(pool: &SqlitePool) -> Result<Vec<PendingApproval>> {
    let rows = sqlx::query(
        "SELECT id, decision_id, action_json, info_json, baseline_json
         FROM pending_approvals ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id")?;
        let decision_id: i64 = row.try_get("decision_id")?;
        let action_json: String = row.try_get("action_json")?;
        let info_json: String = row.try_get("info_json")?;
        let baseline_json: String = row.try_get("baseline_json")?;

        let parsed = (|| -> Result<PendingApproval> {
            let action: FixAction = serde_json::from_str(&action_json)?;
            let mut info: ApprovalInfo = serde_json::from_str(&info_json)?;
            // The row id is the source of truth for the approval id.
            info.id = id as u64;
            let baseline: SystemState = serde_json::from_str(&baseline_json)?;
            Ok(PendingApproval {
                info,
                action,
                decision_id,
                baseline,
            })
        })();

        match parsed {
            Ok(pa) => out.push(pa),
            Err(e) => {
                warn!(id, "Dropping unreadable pending approval: {e}");
                let _ = delete_pending_approval(pool, id as u64).await;
            }
        }
    }
    Ok(out)
}

/// Remove a resolved approval (approved or rejected) from the queue.
pub async fn delete_pending_approval(pool: &SqlitePool, id: u64) -> Result<()> {
    sqlx::query("DELETE FROM pending_approvals WHERE id = ?")
        .bind(id as i64)
        .execute(pool)
        .await?;
    Ok(())
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

/// Delete rows older than `days` from the three high-frequency audit tables that
/// otherwise grow without bound (~52k rows/yr each at the default cadence). The
/// dashboard timeline reads only the last 24h and the trend detector only the last few
/// rows, so a 90-day window keeps every reader whole. Mirrors `feedback::prune_old`;
/// cheap after the first pass (later cycles match nothing). Returns total rows removed.
pub async fn prune_old(pool: &SqlitePool, days: i64) -> anyhow::Result<u64> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let mut removed = 0u64;
    for (table, col) in [
        ("system_state_history", "timestamp"),
        ("execution_log", "executed_at"),
    ] {
        let res = sqlx::query(&format!("DELETE FROM {table} WHERE {col} < ?"))
            .bind(&cutoff)
            .execute(pool)
            .await?;
        removed += res.rows_affected();
    }
    // Prune `decisions` too, but never a decision that still has an outstanding
    // approval: pending_approvals has no time-based retention, so pruning its parent
    // would orphan the queue's decision_id and make a later mark_decision_executed a
    // silent no-op. An unresolved approval keeps its decision row until it's resolved.
    let res = sqlx::query(
        "DELETE FROM decisions WHERE timestamp < ? \
         AND id NOT IN (SELECT decision_id FROM pending_approvals)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;
    removed += res.rows_affected();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(vals: &[(f64, f64, f64)]) -> Vec<MetricSample> {
        vals.iter()
            .map(|&(cpu, mem, disk)| MetricSample { cpu, mem, disk })
            .collect()
    }

    fn mp(at: i64) -> eir_proto::MetricPoint {
        eir_proto::MetricPoint {
            at,
            cpu: 1.0,
            memory: 2.0,
            disk: 3.0,
        }
    }

    #[test]
    fn thin_points_caps_and_keeps_endpoints() {
        let pts: Vec<_> = (0..1000).map(mp).collect();
        let out = thin_points(pts, 200);
        assert_eq!(out.len(), 200);
        assert_eq!(out.first().unwrap().at, 0);
        assert_eq!(out.last().unwrap().at, 999);
        // Monotonic non-decreasing timestamps preserved.
        assert!(out.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn thin_points_leaves_small_series_untouched() {
        let pts: Vec<_> = (0..10).map(mp).collect();
        assert_eq!(thin_points(pts.clone(), 200).len(), 10);
        // cap < 2 is a no-op guard (never happens in practice; cap is 200).
        assert_eq!(thin_points(pts, 1).len(), 10);
    }

    #[test]
    fn trend_needs_a_full_window() {
        // Fewer than 6 samples → no trend, however steep.
        let s = samples(&[(10.0, 10.0, 10.0), (90.0, 90.0, 90.0)]);
        assert!(summarise_trend(&s).is_none());
    }

    #[test]
    fn steady_healthy_metrics_are_not_a_trend() {
        let s = samples(&[(20.0, 40.0, 50.0); 8]);
        assert!(summarise_trend(&s).is_none());
    }

    #[test]
    fn a_filling_disk_is_flagged() {
        // Disk climbs from ~74 to ~80 and is elevated → flagged.
        let s = samples(&[
            (10.0, 40.0, 73.0),
            (10.0, 40.0, 74.0),
            (10.0, 40.0, 75.0),
            (10.0, 40.0, 79.0),
            (10.0, 40.0, 80.0),
            (10.0, 40.0, 81.0),
        ]);
        let out = summarise_trend(&s).expect("should flag a filling disk");
        assert!(out.contains("disk usage trending up"));
    }

    #[test]
    fn a_rising_but_low_disk_is_not_flagged() {
        // Climbing, but nowhere near full → not flagged (avoids noise).
        let s = samples(&[
            (10.0, 40.0, 20.0),
            (10.0, 40.0, 22.0),
            (10.0, 40.0, 24.0),
            (10.0, 40.0, 26.0),
            (10.0, 40.0, 28.0),
            (10.0, 40.0, 30.0),
        ]);
        assert!(summarise_trend(&s).is_none());
    }
}
