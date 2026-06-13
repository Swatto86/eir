use crate::models::FixAction;
use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// Returns true if this action was successfully executed within the rate-limit window.
pub async fn rate_limited(pool: &SqlitePool, action: &FixAction, window_mins: u32) -> Result<bool> {
    let key = action_key(action);
    let cutoff = (Utc::now() - chrono::Duration::minutes(window_mins as i64)).to_rfc3339();

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM execution_log WHERE action LIKE ? AND executed_at > ? AND success = 1",
    )
    .bind(format!("%{key}%"))
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

fn action_key(action: &FixAction) -> String {
    match action {
        FixAction::ServiceRestart { service_name } => format!("ServiceRestart({service_name})"),
        FixAction::ServiceStop { service_name } => format!("ServiceStop({service_name})"),
        FixAction::ServiceStart { service_name } => format!("ServiceStart({service_name})"),
        FixAction::LogCleanup { path, .. } => format!("LogCleanup({path})"),
        FixAction::DiskCleanup { target } => format!("DiskCleanup({target})"),
        FixAction::PowerShellDiagnostic { .. } => "PowerShellDiagnostic".to_string(),
        FixAction::TaskDisable { task_name } => format!("TaskDisable({task_name})"),
        FixAction::TaskEnable { task_name } => format!("TaskEnable({task_name})"),
        FixAction::RegistryReset {
            key_path,
            value_name,
            ..
        } => {
            format!("RegistryReset({key_path}/{value_name})")
        }
        FixAction::NetworkDiagnostic { command } => format!("NetworkDiagnostic({command})"),
        FixAction::DriverDisable { driver_name } => format!("DriverDisable({driver_name})"),
        FixAction::DriverEnable { driver_name } => format!("DriverEnable({driver_name})"),
        FixAction::SoftwareUninstall { package_name } => {
            format!("SoftwareUninstall({package_name})")
        }
        FixAction::BcdEdit { element, value } => format!("BcdEdit({element}={value})"),
        FixAction::ProcessKill { process_name } => format!("ProcessKill({process_name})"),
        FixAction::FileDelete { path } => format!("FileDelete({path})"),
    }
}
