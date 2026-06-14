pub mod boot;
pub mod driver;
pub mod logs;
pub mod powershell;
pub mod process;
pub mod registry;
pub mod services;
pub mod software;
pub mod tasks;

use crate::models::{ExecutionResult, FixAction};
use tracing::{error, info};

pub async fn execute(action: &FixAction) -> ExecutionResult {
    info!("Executing: {action:?}");

    match action {
        FixAction::ServiceRestart { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::restart(&n)).await
        }
        FixAction::ServiceStop { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::stop(&n)).await
        }
        FixAction::ServiceStart { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::start(&n)).await
        }
        FixAction::LogCleanup { path, days_old } => {
            let (p, d) = (path.clone(), *days_old);
            blocking(action, move || logs::cleanup(&p, d)).await
        }
        FixAction::DiskCleanup { target } => {
            let script = match target.to_lowercase().as_str() {
                "temp" | "tmp" => {
                    "Remove-Item -Path \"$env:TEMP\\*\" -Recurse -Force -ErrorAction SilentlyContinue; \
                     Write-Output 'Temp folder cleaned'"
                }
                "prefetch" => {
                    "Remove-Item -Path 'C:\\Windows\\Prefetch\\*' -Force -ErrorAction SilentlyContinue; \
                     Write-Output 'Prefetch cleaned'"
                }
                _ => "Write-Output 'Unknown disk cleanup target — no action taken'",
            };
            make_result(action, powershell::run_diagnostic(script).await)
        }
        FixAction::PowerShellDiagnostic { script } => {
            make_result(action, powershell::run_diagnostic(script).await)
        }
        FixAction::TaskDisable { task_name } => {
            let n = task_name.clone();
            blocking(action, move || tasks::disable(&n)).await
        }
        FixAction::TaskEnable { task_name } => {
            let n = task_name.clone();
            blocking(action, move || tasks::enable(&n)).await
        }
        FixAction::RegistryReset {
            key_path,
            value_name,
            value_data,
        } => {
            let (k, v, d) = (key_path.clone(), value_name.clone(), value_data.clone());
            blocking(action, move || registry::reset_value(&k, &v, &d)).await
        }
        FixAction::NetworkDiagnostic { command } => {
            let script = match command.to_lowercase().as_str() {
                "flush_dns" => "ipconfig /flushdns",
                "release_renew" => "ipconfig /release; Start-Sleep -Seconds 2; ipconfig /renew",
                "reset_tcp" => "netsh int ip reset",
                "reset_winsock" => "netsh winsock reset",
                other => {
                    let msg = format!("Unknown network diagnostic command: '{other}'");
                    error!("{msg}");
                    return ExecutionResult {
                        action: format!("{action:?}"),
                        success: false,
                        output: msg,
                    };
                }
            };
            make_result(action, powershell::run_diagnostic(script).await)
        }
        FixAction::DriverDisable { driver_name } => {
            let n = driver_name.clone();
            make_result(action, driver::disable(&n).await)
        }
        FixAction::DriverEnable { driver_name } => {
            let n = driver_name.clone();
            make_result(action, driver::enable(&n).await)
        }
        FixAction::SoftwareUninstall { package_name } => {
            let n = package_name.clone();
            make_result(action, software::uninstall(&n).await)
        }
        FixAction::BcdEdit { element, value } => {
            let (el, val) = (element.clone(), value.clone());
            make_result(action, boot::bcd_edit(&el, &val).await)
        }
        FixAction::ProcessKill { process_name } => {
            let n = process_name.clone();
            make_result(action, process::kill(&n).await)
        }
        FixAction::FileDelete { path } => {
            let safe = path.replace('\'', "''");
            // Guard: refuse if path is a directory, and require the item to exist.
            let script = format!(
                r#"$item = Get-Item -LiteralPath '{safe}' -ErrorAction SilentlyContinue
if (-not $item) {{ Write-Output 'Not found (already gone?): {safe}' }}
elseif ($item.PSIsContainer) {{ throw 'Refusing to delete directory: {safe}' }}
else {{ Remove-Item -LiteralPath '{safe}' -Force -ErrorAction Stop; Write-Output 'Deleted: {safe}' }}"#
            );
            make_result(action, powershell::run_diagnostic(&script).await)
        }
    }
}

async fn blocking(
    action: &FixAction,
    f: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
) -> ExecutionResult {
    let r = tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
    make_result(action, r)
}

fn make_result(action: &FixAction, r: anyhow::Result<String>) -> ExecutionResult {
    let label = format!("{action:?}");
    match r {
        Ok(msg) => {
            info!(action = %label, output = %msg, "Execution succeeded");
            ExecutionResult {
                action: label,
                success: true,
                output: msg,
            }
        }
        Err(e) => {
            error!(action = %label, error = %e, "Execution failed");
            ExecutionResult {
                action: label,
                success: false,
                output: e.to_string(),
            }
        }
    }
}
