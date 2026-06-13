use anyhow::Result;

pub fn disable(task_name: &str) -> Result<String> {
    run_task_cmd("Disable-ScheduledTask", task_name)
}

pub fn enable(task_name: &str) -> Result<String> {
    run_task_cmd("Enable-ScheduledTask", task_name)
}

fn run_task_cmd(cmdlet: &str, task_name: &str) -> Result<String> {
    let safe_name = task_name.replace('\'', "''");
    let script = format!(
        "{cmdlet} -TaskName '{safe_name}' -ErrorAction Stop | Out-Null; \
         Write-Output \"{cmdlet} succeeded for '{safe_name}'\""
    );

    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if out.status.success() {
        Ok(stdout.trim().to_string())
    } else {
        anyhow::bail!("{cmdlet} failed for '{task_name}': {stderr}")
    }
}
