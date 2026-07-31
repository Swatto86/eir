//! Bounded subprocess execution for the updater.
//!
//! winget/choco/scoop launched from the LocalSystem service can hang indefinitely
//! (winget's source refresh as SYSTEM is a known case). An unbounded `Command::output()`
//! then wedges the whole update cycle, and because no `CycleSummary` is ever produced
//! the updater's "running" state latches forever. Every external command the updater
//! runs goes through [`run_capped`]/[`run_capped_cmd`], which terminates the child if
//! it overruns its deadline (`kill_on_drop` fires when the timed-out future is dropped).

use std::process::Command as StdCommand;
use std::time::Duration;

/// CREATE_NO_WINDOW — keep spawned consoles hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Exit code reported when a command overran its deadline and was terminated. Negative
/// so it can never collide with a real Windows exit code; the accompanying "timed out"
/// text classifies as a transient (non-terminal) failure, so the self-heal moves on to
/// another method rather than giving up.
pub const TIMED_OUT: i32 = -4;

/// Quick presence probes (`where`, `--version`).
/// Listing available updates — a winget/choco/scoop query whose source refresh can be slow.
pub const LIST: Duration = Duration::from_secs(150);
/// Applying one update: download + run an installer.
pub const INSTALL: Duration = Duration::from_secs(600);
/// Reading an installed version back to confirm an update took effect.
pub const VERIFY: Duration = Duration::from_secs(60);

/// Run a prepared command with a hard timeout, capturing merged stdout+stderr. On
/// timeout the child is killed (via `kill_on_drop`) and `(TIMED_OUT, explanation)` is
/// returned. Callers that need custom env/program paths build the [`StdCommand`]
/// themselves and hand it over; creation flags are applied here.
pub async fn run_capped_cmd(mut cmd: StdCommand, dur: Duration) -> (i32, String) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    match tokio::time::timeout(dur, cmd.output()).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr);
            if !e.trim().is_empty() {
                s.push('\n');
                s.push_str(e.trim());
            }
            (o.status.code().unwrap_or(-1), s)
        }
        Ok(Err(e)) => (-1, format!("could not launch command: {e}")),
        Err(_) => (
            TIMED_OUT,
            format!(
                "command timed out after {}s and was terminated",
                dur.as_secs()
            ),
        ),
    }
}

/// Convenience for the common `(program, args)` case.
pub async fn run_capped(program: &str, args: &[String], dur: Duration) -> (i32, String) {
    let mut c = StdCommand::new(program);
    c.args(args);
    run_capped_cmd(c, dur).await
}

/// Reject a failed read-only command instead of letting its empty/partial output
/// masquerade as a successful inventory with no updates.
pub fn checked_output<'a>(action: &str, code: i32, output: &'a str) -> Result<&'a str, String> {
    if code == 0 {
        return Ok(output);
    }
    let detail: String = output.trim().chars().take(300).collect();
    Err(if detail.is_empty() {
        format!("{action} failed (exit {code})")
    } else {
        format!("{action} failed (exit {code}): {detail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fast_command_returns_its_output() {
        let (code, out) = run_capped(
            "cmd",
            &["/c".into(), "echo".into(), "eir-proc-test".into()],
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(code, 0);
        assert!(out.contains("eir-proc-test"), "got: {out:?}");
    }

    #[tokio::test]
    async fn overrunning_command_is_killed_and_flagged() {
        // `ping -n 4` runs ~3s; a 300ms cap must terminate it and report TIMED_OUT,
        // proving a hung child can't wedge the cycle.
        let (code, out) = run_capped(
            "cmd",
            &[
                "/c".into(),
                "ping".into(),
                "-n".into(),
                "4".into(),
                "127.0.0.1".into(),
            ],
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(code, TIMED_OUT);
        assert!(out.contains("timed out"), "got: {out:?}");
    }

    #[test]
    fn failed_listing_is_not_an_empty_success() {
        let err = checked_output("winget update listing", 1, "").expect_err("must fail");
        assert!(err.contains("winget update listing"));
        assert!(err.contains("exit 1"));
    }
}
