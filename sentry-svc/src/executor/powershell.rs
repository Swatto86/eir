use anyhow::Result;
use tokio::process::Command;

/// Run a read-only diagnostic PowerShell script and return its output.
/// Script is always run with -NonInteractive and Bypass execution policy.
pub async fn run_diagnostic(script: &str) -> Result<String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(stdout.to_string())
    } else {
        Err(anyhow::anyhow!(
            "Script exited with code {:?}\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ))
    }
}
