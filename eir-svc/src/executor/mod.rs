pub mod boot;
pub mod driver;
pub mod logs;
pub mod powershell;
pub mod process;
pub mod registry;
pub mod repair;
pub mod security;
pub mod services;
pub mod software;
pub mod startup;
pub mod tasks;

use crate::models::{ExecutionResult, FixAction};
use tracing::{error, info};

fn build_disk_cleanup_script(target: &str) -> Option<String> {
    let (root, label) = match target.to_ascii_lowercase().as_str() {
        "temp" | "tmp" => (r"C:\Windows\Temp", "Temp folder"),
        "prefetch" => (r"C:\Windows\Prefetch", "Prefetch"),
        _ => return None,
    };
    let root = powershell::ps_single_quote(root);
    Some(format!(
        "$ErrorActionPreference='Stop'; $root={root}; \
         if (-not (Test-Path -LiteralPath $root -PathType Container -ErrorAction Stop)) {{ \
           Write-Output '{label} is already empty'; return \
         }}; \
         $items=@(Get-ChildItem -LiteralPath $root -Force -ErrorAction Stop); \
         $removed=0; $skipped=0; \
         foreach($item in $items) {{ \
           try {{ \
             Remove-Item -LiteralPath $item.FullName -Recurse -Force -ErrorAction Stop; \
             if (Test-Path -LiteralPath $item.FullName -ErrorAction Stop) {{ \
               throw 'item remained after deletion' \
             }}; \
             $removed++ \
           }} catch {{ $skipped++ }} \
         }}; \
         if ($items.Count -gt 0 -and $removed -eq 0) {{ \
           throw '{label} cleanup could not remove any item' \
         }}; \
         Write-Output ('{label} cleaned: {{0}} removed, {{1}} skipped' -f $removed,$skipped)"
    ))
}

fn network_diagnostic_script(command: &str) -> Option<&'static str> {
    match command.to_ascii_lowercase().as_str() {
        "flush_dns" => Some(
            "ipconfig /flushdns; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig flushdns failed' }",
        ),
        "release_renew" => Some(
            "ipconfig /release; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig release failed' }; \
             Start-Sleep -Seconds 2; ipconfig /renew; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig renew failed' }",
        ),
        "reset_tcp" => Some(
            "netsh int ip reset; \
             if ($LASTEXITCODE -ne 0) { throw 'TCP/IP reset failed' }",
        ),
        "reset_winsock" => Some(
            "netsh winsock reset; \
             if ($LASTEXITCODE -ne 0) { throw 'Winsock reset failed' }",
        ),
        _ => None,
    }
}

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
            let script = match build_disk_cleanup_script(target) {
                Some(script) => script,
                None => {
                    // Report a real failure, not a success-shaped no-op — disk_cleanup is
                    // auto-executed, so a false "success" here would poison the rate limiter
                    // (safety::rate_limited suppresses a fingerprint that already "succeeded").
                    // Mirrors the NetworkDiagnostic unknown-command branch below.
                    let msg = format!("Unknown disk cleanup target: '{target}'");
                    error!("{msg}");
                    return ExecutionResult {
                        action: format!("{action:?}"),
                        success: false,
                        output: msg,
                        undo: None,
                    };
                }
            };
            make_result(action, powershell::run_diagnostic(&script).await)
        }
        FixAction::PowerShellDiagnostic { script } => {
            make_result(action, powershell::run_diagnostic(script).await)
        }
        FixAction::TaskDisable { task_name } => {
            make_result(action, tasks::disable(task_name).await)
        }
        FixAction::TaskEnable { task_name } => make_result(action, tasks::enable(task_name).await),
        FixAction::RegistryReset {
            key_path,
            value_name,
            value_data,
        } => match registry::reset_value(key_path, value_name, value_data).await {
            Ok((msg, undo)) => {
                let mut res = make_result(action, Ok(msg));
                res.undo = undo;
                res
            }
            Err(e) => make_result(action, Err(e)),
        },
        FixAction::NetworkDiagnostic { command } => {
            let script = match network_diagnostic_script(command) {
                Some(script) => script,
                None => {
                    let msg = format!("Unknown network diagnostic command: '{command}'");
                    error!("{msg}");
                    return ExecutionResult {
                        action: format!("{action:?}"),
                        success: false,
                        output: msg,
                        undo: None,
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
        FixAction::FirewallEnable { profile } => {
            let p = profile.clone();
            make_result(action, security::firewall_enable(&p).await)
        }
        FixAction::DefenderSignatureUpdate => {
            make_result(action, security::defender_signature_update().await)
        }
        FixAction::DefenderRealtimeEnable => {
            make_result(action, security::defender_realtime_enable().await)
        }
        FixAction::SfcScan => make_result(action, repair::sfc_scan().await),
        FixAction::DismRestoreHealth => make_result(action, repair::dism_restore_health().await),
        FixAction::StartupSet {
            name,
            location,
            hive,
            enable,
        } => make_result(
            action,
            startup::set_enabled(name, location, hive, *enable).await,
        ),
        FixAction::FileDelete { path } => {
            let path = path.clone();
            blocking(action, move || logs::delete_file(&path)).await
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
                undo: None,
            }
        }
        Err(e) => {
            error!(action = %label, error = %e, "Execution failed");
            ExecutionResult {
                action: label,
                success: false,
                output: e.to_string(),
                undo: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_disk_cleanup_script, network_diagnostic_script};

    #[test]
    fn cleanup_and_network_scripts_fail_when_native_effects_fail() {
        let cleanup = build_disk_cleanup_script("temp").expect("known target");
        assert!(cleanup.contains("C:\\Windows\\Temp"));
        assert!(cleanup.contains("Test-Path"));
        assert!(cleanup.contains("$removed -eq 0"));
        assert!(!cleanup.contains("SilentlyContinue"));
        for command in ["flush_dns", "release_renew", "reset_tcp", "reset_winsock"] {
            assert!(network_diagnostic_script(command)
                .expect("known command")
                .contains("$LASTEXITCODE"));
        }
    }
}
