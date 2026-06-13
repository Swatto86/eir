pub mod logs;
pub mod powershell;
pub mod services;

use crate::models::{ExecutionResult, FixAction};
use tracing::info;

pub async fn execute(action: &FixAction) -> ExecutionResult {
    info!("Executing: {action:?}");

    match action {
        FixAction::ServiceRestart { service_name } => {
            let name = service_name.clone();
            let r = tokio::task::spawn_blocking(move || services::restart(&name))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
            make_result(action, r)
        }
        FixAction::ServiceStop { service_name } => {
            let name = service_name.clone();
            let r = tokio::task::spawn_blocking(move || services::stop(&name))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
            make_result(action, r)
        }
        FixAction::ServiceStart { service_name } => {
            let name = service_name.clone();
            let r = tokio::task::spawn_blocking(move || services::start(&name))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
            make_result(action, r)
        }
        FixAction::LogCleanup { path, days_old } => {
            let (p, d) = (path.clone(), *days_old);
            let r = tokio::task::spawn_blocking(move || logs::cleanup(&p, d))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
            make_result(action, r)
        }
        FixAction::DiskCleanup { target } => {
            let script = match target.to_lowercase().as_str() {
                "temp" | "tmp" => "Remove-Item -Path $env:TEMP\\* -Recurse -Force -ErrorAction SilentlyContinue; Write-Output 'Temp cleaned'",
                "prefetch" => "Remove-Item -Path C:\\Windows\\Prefetch\\* -Force -ErrorAction SilentlyContinue; Write-Output 'Prefetch cleaned'",
                _ => "Write-Output 'Unknown disk cleanup target — no action taken'",
            };
            let r = powershell::run_diagnostic(script).await;
            make_result(action, r)
        }
        FixAction::PowerShellDiagnostic { script } => {
            let r = powershell::run_diagnostic(script).await;
            make_result(action, r)
        }
    }
}

fn make_result(action: &FixAction, r: anyhow::Result<String>) -> ExecutionResult {
    let action_str = format!("{action:?}");
    let (success, output) = match r {
        Ok(msg) => {
            info!(action = %action_str, "Execution succeeded: {msg}");
            (true, msg)
        }
        Err(e) => {
            tracing::error!(action = %action_str, "Execution failed: {e}");
            (false, e.to_string())
        }
    };
    ExecutionResult { action: action_str, success, output }
}
