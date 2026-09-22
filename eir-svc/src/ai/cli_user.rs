//! LocalSystem → active-user CLI launch. Winget and subscription CLIs must not
//! run with SYSTEM authority against user-writable binaries.

#[cfg(not(windows))]
use anyhow::bail;
use anyhow::{Context, Result};

#[cfg(windows)]
#[path = "cli_user_launch.rs"]
mod launch;

#[cfg(windows)]
pub(crate) use launch::{run_cli_as_active_user, running_as_local_system, UserCliSpec};

#[cfg(not(windows))]
pub(crate) fn running_as_local_system() -> bool {
    false
}

#[cfg(windows)]
fn configured_binary(configured: Option<&str>, _profile: Option<&str>) -> String {
    configured.unwrap_or_default().to_string()
}

/// Run machine Winget with the active desktop user's primary token.
#[cfg(windows)]
pub(crate) async fn run_winget_as_active_user(
    program: &std::path::Path,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<(i32, String)> {
    static USER_PROGRAM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let program = program.to_string_lossy().into_owned();
    let args = args.to_vec();
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let seq = USER_PROGRAM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let output = tokio::task::spawn_blocking(move || {
        run_cli_as_active_user(
            UserCliSpec {
                configured_binary: Some(&program),
                resolve_binary: configured_binary,
                what: "winget",
                scratch_prefix: "eir-winget",
                workspace_flag: None,
                workspace_files: |_| Vec::new(),
                timeout_ms,
            },
            &args,
            "",
            &[],
            seq,
        )
    })
    .await
    .context("Join active-user winget task")??;
    let mut merged = output.stdout;
    if !output.stderr.trim().is_empty() {
        merged.push('\n');
        merged.push_str(output.stderr.trim());
    }
    Ok((i32::from_ne_bytes(output.code.to_ne_bytes()), merged))
}

#[cfg(not(windows))]
pub(crate) async fn run_winget_as_active_user(
    _program: &std::path::Path,
    _args: &[String],
    _timeout: std::time::Duration,
) -> Result<(i32, String)> {
    bail!("winget active-user launch is Windows-only")
}

#[cfg(all(test, windows))]
mod tests {
    use super::launch::{user_cli_command_line, CliRedirections};

    #[test]
    fn user_cli_command_line_quotes_metacharacters_and_rejects_expansion() {
        let args = vec!["--model".to_string(), "safe&literal".to_string()];
        let line = user_cli_command_line(
            r"C:\Windows\System32\cmd.exe",
            r"C:\Program Files\Codex\codex.exe",
            &args,
            CliRedirections {
                profile: r"C:\Users\Owner",
                stdin: std::path::Path::new(r"C:\Users\Owner\in.txt"),
                stdout: std::path::Path::new(r"C:\Users\Owner\out.txt"),
                stderr: std::path::Path::new(r"C:\Users\Owner\err.txt"),
            },
        )
        .unwrap();
        assert!(line.contains(r#""safe&literal""#));
        assert!(user_cli_command_line(
            r"C:\Windows\System32\cmd.exe",
            r"C:\Users\%USERNAME%\codex.exe",
            &[],
            CliRedirections {
                profile: r"C:\Users\Owner",
                stdin: std::path::Path::new(r"C:\in"),
                stdout: std::path::Path::new(r"C:\out"),
                stderr: std::path::Path::new(r"C:\err"),
            },
        )
        .is_err());
    }
}
