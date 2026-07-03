use anyhow::Result;
use std::time::Duration;
use tokio::process::Command;

/// Default ceiling for a PowerShell action. Bounds the decision loop: actions run
/// inline, so an unbounded script could otherwise wedge the loop forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Quote a value as a PowerShell **single-quoted** string literal: wrap in single
/// quotes and double any embedded single quote. This is the only safe way to embed
/// untrusted data (AI-supplied names/paths) in a PowerShell command — a double-quoted
/// string would still evaluate `$(...)`, `$var`, and backtick escapes. Returns the
/// value INCLUDING its surrounding quotes, e.g. `ps_single_quote("a'b") == "'a''b'"`.
pub fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Run a PowerShell script with the default timeout and return its output.
/// Script is always run with -NonInteractive and Bypass execution policy.
pub async fn run_diagnostic(script: &str) -> Result<String> {
    run_diagnostic_with_timeout(script, DEFAULT_TIMEOUT).await
}

/// As [`run_diagnostic`] but with an explicit timeout, for actions that can
/// legitimately run longer than the default (e.g. a Defender signature pull).
pub async fn run_diagnostic_with_timeout(script: &str, timeout: Duration) -> Result<String> {
    let child = Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Kill the powershell process if we hit the timeout and drop the future.
        .kill_on_drop(true)
        .spawn()?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(res) => res?,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Script timed out after {}s",
                timeout.as_secs()
            ));
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_wraps_and_doubles_quotes() {
        assert_eq!(ps_single_quote("plain"), "'plain'");
        assert_eq!(ps_single_quote("O'Brien"), "'O''Brien'");
        // A subexpression payload is inert inside a single-quoted literal.
        assert_eq!(ps_single_quote("$(calc.exe)"), "'$(calc.exe)'");
    }
}
