use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Default ceiling for a PowerShell action. Bounds the decision loop: actions run
/// inline, so an unbounded script could otherwise wedge the loop forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DIAGNOSTIC_STREAM_BYTES: usize = 1024 * 1024;

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
    let mut child = Command::new("powershell.exe")
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

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let completed = async {
        let (stdout, stderr, status) =
            tokio::join!(read_capped(stdout), read_capped(stderr), child.wait(),);
        Ok::<_, anyhow::Error>((status?, stdout?, stderr?))
    };
    let (status, (stdout, stdout_truncated), (stderr, stderr_truncated)) =
        match tokio::time::timeout(timeout, completed).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Script timed out after {}s",
                    timeout.as_secs()
                ));
            }
        };

    let mut stdout = String::from_utf8_lossy(&stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if stdout_truncated {
        stdout.push_str(&format!(
            "\n[stdout truncated at {MAX_DIAGNOSTIC_STREAM_BYTES} bytes]"
        ));
    }
    if stderr_truncated {
        stderr.push_str(&format!(
            "\n[stderr truncated at {MAX_DIAGNOSTIC_STREAM_BYTES} bytes]"
        ));
    }

    if status.success() {
        Ok(stdout)
    } else {
        Err(anyhow::anyhow!(
            "Script exited with code {:?}\nstdout: {stdout}\nstderr: {stderr}",
            status.code()
        ))
    }
}

async fn read_capped<R: AsyncRead + Unpin>(stream: Option<R>) -> std::io::Result<(Vec<u8>, bool)> {
    let Some(mut stream) = stream else {
        return Ok((Vec::new(), false));
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let retained = read.min(MAX_DIAGNOSTIC_STREAM_BYTES.saturating_sub(output.len()));
        output.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok((output, truncated))
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

    #[tokio::test]
    async fn diagnostic_output_is_retained_with_a_fixed_cap() {
        let script = format!(
            "[Console]::Out.Write(('x' * {}))",
            MAX_DIAGNOSTIC_STREAM_BYTES + 4096
        );
        let output = run_diagnostic_with_timeout(&script, Duration::from_secs(15))
            .await
            .expect("diagnostic");
        assert!(output.len() <= MAX_DIAGNOSTIC_STREAM_BYTES + 128);
        assert!(output.contains("truncated"));
    }
}
