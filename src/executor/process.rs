use anyhow::{bail, Result};

const PROTECTED_PROCESSES: &[&str] = &[
    "System", "Idle", "lsass", "winlogon", "csrss",
    "smss", "wininit", "services", "ntoskrnl",
];

pub async fn kill(process_name: &str) -> Result<String> {
    let lower = process_name.to_lowercase();
    if PROTECTED_PROCESSES
        .iter()
        .any(|&p| p.to_lowercase() == lower)
    {
        bail!("Refusing to kill protected process: {process_name}");
    }
    let safe_name = process_name.replace('\'', "''");
    let script = format!(
        "Stop-Process -Name '{safe_name}' -Force -ErrorAction SilentlyContinue; \
         Write-Output 'Kill signal sent to: {safe_name}'"
    );
    super::powershell::run_diagnostic(&script).await
}
