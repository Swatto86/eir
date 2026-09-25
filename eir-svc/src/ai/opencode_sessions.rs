//! OpenCode session cleanup. Every `opencode run` stores a session holding the whole prompt
//! (a multi-megabyte system snapshot) in the desktop user's `opencode.db`, and Eir never
//! resumes one, so without cleanup the database grew by gigabytes a fortnight. After each
//! run the session is deleted with `opencode session delete <id>`, as the same user.
//!
//! The id comes from the run's NDJSON (`sessionID` on every event). A run killed before it
//! printed anything still created a session, so when no id was printed the session is found
//! by its scratch directory name (`eir-opencode-<pid>-<seq>`, unique per run).

use crate::ai::cli_process::{cli_process, wait_capped, CliProcessOutput};
use crate::ai::cli_user::{run_cli_as_active_user, running_as_local_system, UserCliSpec};
use crate::ai::opencode_cli::resolve_opencode_binary;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;
use tracing::{info, warn};

/// Bound on each `session list` / `session delete` call (OpenCode takes a few seconds to start).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// `session list` only shows the project of its working directory; Eir's runs use scratch
/// directories under the temp folder, so it runs there, and only recent sessions matter.
const LIST_ARGS: [&str; 6] = ["session", "list", "--format", "json", "-n", "50"];
/// Bound on the whole cleanup of one run.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything needed to find and delete one run's session.
pub(crate) struct CleanupTarget {
    pub configured_binary: Option<String>,
    pub user_profile: Option<String>,
    /// The run's scratch directory name, e.g. `eir-opencode-1234-7`.
    pub scratch_name: String,
    /// The run's stdout, when the run produced any.
    pub stdout: Option<String>,
    pub seq: u64,
}

/// OpenCode ids look like `ses_f2b76d913ffe5jWgNWp9V6HvcE`. Anything else is refused before it
/// can reach a command line.
fn valid_session_id(id: &str) -> bool {
    id.starts_with("ses_")
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn push_unique(ids: &mut Vec<String>, id: &str) {
    if valid_session_id(id) && !ids.iter().any(|x| x == id) {
        ids.push(id.to_string());
    }
}

/// Session ids named in a run's NDJSON, in first-seen order (pure, unit-tested).
pub(crate) fn session_ids_from_ndjson(stdout: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in stdout.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        if let Ok(ev) = serde_json::from_str::<Value>(line) {
            if let Some(id) = ev["sessionID"].as_str() {
                push_unique(&mut ids, id);
            }
        }
    }
    ids
}

/// Sessions from `opencode session list --format json` whose directory is this run's scratch
/// directory (compared by final path component, case-insensitively) (pure, unit-tested).
pub(crate) fn session_ids_for_scratch(list_json: &str, scratch_name: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(Value::Array(sessions)) = serde_json::from_str::<Value>(list_json.trim()) else {
        return ids;
    };
    for s in &sessions {
        let dir = s["directory"].as_str().unwrap_or_default();
        let last = dir.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next();
        if last.is_some_and(|name| name.eq_ignore_ascii_case(scratch_name)) {
            if let Some(id) = s["id"].as_str() {
                push_unique(&mut ids, id);
            }
        }
    }
    ids
}

/// Delete the run's session in the background. Never fails or delays the run itself; any
/// problem is logged.
pub(crate) fn spawn_cleanup(target: CleanupTarget) {
    tokio::spawn(async move {
        let scratch = target.scratch_name.clone();
        match tokio::time::timeout(CLEANUP_TIMEOUT, cleanup(target)).await {
            Ok(Ok(0)) => warn!(%scratch, "No OpenCode session found to delete for this run"),
            Ok(Ok(n)) => info!(%scratch, deleted = n, "Deleted the OpenCode session for this run"),
            Ok(Err(e)) => warn!(%scratch, "OpenCode session cleanup failed: {e:#}"),
            Err(_) => warn!(%scratch, "OpenCode session cleanup timed out"),
        }
    });
}

async fn cleanup(target: CleanupTarget) -> Result<usize> {
    let mut ids = target
        .stdout
        .as_deref()
        .map(session_ids_from_ndjson)
        .unwrap_or_default();
    if ids.is_empty() {
        let listed = run_opencode(&target, &LIST_ARGS).await?;
        if listed.code != 0 {
            bail!("session list exited with code {}", listed.code);
        }
        ids = session_ids_for_scratch(&listed.stdout, &target.scratch_name);
    }
    let mut deleted = 0;
    for id in &ids {
        let out = run_opencode(&target, &["session", "delete", id]).await?;
        if out.code == 0 {
            deleted += 1;
        } else {
            warn!(
                id = %id,
                code = out.code,
                "opencode session delete failed: {}",
                out.stderr.trim()
            );
        }
    }
    Ok(deleted)
}

/// Run one short OpenCode command as the same user, and with the same profile, as the run.
async fn run_opencode(target: &CleanupTarget, args: &[&str]) -> Result<CliProcessOutput> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    if running_as_local_system() {
        let binary = target.configured_binary.clone();
        let seq = target.seq;
        return tokio::task::spawn_blocking(move || {
            run_cli_as_active_user(
                UserCliSpec {
                    configured_binary: binary.as_deref(),
                    resolve_binary: resolve_opencode_binary,
                    what: "opencode session cleanup",
                    scratch_prefix: "eir-opencode-cleanup",
                    workspace_flag: None,
                    workspace_files: |_| Vec::new(),
                    timeout_ms: 30_000,
                },
                &args,
                "",
                &[],
                seq,
            )
        })
        .await
        .context("Join OpenCode cleanup task")?;
    }
    let configured = target.configured_binary.clone();
    let profile = target.user_profile.clone();
    let binary = tokio::task::spawn_blocking(move || {
        resolve_opencode_binary(configured.as_deref(), profile.as_deref())
    })
    .await
    .context("Join OpenCode binary resolution task")?;
    let mut command = cli_process(&binary);
    command
        .args(&args)
        .current_dir(std::env::temp_dir())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(profile) = target.user_profile.as_deref() {
        command
            .env("USERPROFILE", profile)
            .env("HOME", profile)
            .env("APPDATA", format!("{profile}\\AppData\\Roaming"))
            .env("LOCALAPPDATA", format!("{profile}\\AppData\\Local"));
    }
    let child = command
        .spawn()
        .context("Start opencode for session cleanup")?;
    let (status, stdout, stderr) = tokio::time::timeout(
        COMMAND_TIMEOUT,
        wait_capped(child, "opencode session cleanup", None),
    )
    .await
    .context("opencode session cleanup command timed out")??;
    Ok(CliProcessOutput {
        code: status.code().map(|c| c as u32).unwrap_or(u32::MAX),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real capture: a failed run (unreachable model, exit code 1).
    const FAILED_RUN: &str = r#"{"type":"error","timestamp":1790272153725,"sessionID":"ses_f2b76d913ffe5jWgNWp9V6HvcE","error":{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_4b8387e6"}}}"#;

    #[test]
    fn ids_come_from_every_run_shape_once_each() {
        assert_eq!(
            session_ids_from_ndjson(FAILED_RUN),
            vec!["ses_f2b76d913ffe5jWgNWp9V6HvcE"]
        );
        let success = r#"
Some log preamble
{"type":"step_start","sessionID":"ses_abc123","part":{"type":"step-start"}}
{"type":"text","sessionID":"ses_abc123","part":{"type":"text","text":"hi"}}
not json {"sessionID":"ses_ignored"}
{"type":"step_finish","sessionID":"ses_abc123","part":{"type":"step-finish"}}
"#;
        assert_eq!(session_ids_from_ndjson(success), vec!["ses_abc123"]);
        assert!(
            session_ids_from_ndjson("").is_empty(),
            "a killed run prints nothing"
        );
        assert!(session_ids_from_ndjson("garbage\n{broken").is_empty());
    }

    /// Real OpenCode check (needs the `opencode` CLI; spends no model call because the model
    /// does not exist): a failed run's session must be gone afterwards.
    /// `cargo test -p eir-svc -- --ignored real_opencode`
    #[tokio::test]
    #[ignore = "needs the opencode CLI installed for this user"]
    async fn real_opencode_run_leaves_no_session_behind() {
        // Same profile resolution as AiClient builds for a non-service run.
        let profile = crate::ai::opencode_cli::resolve_opencode_profile(None);
        let result = crate::ai::opencode_cli::call_opencode_cli(
            None,
            "llamacpp/eir-test-does-not-exist",
            "",
            profile.as_deref(),
            "Reply with OK.",
            false,
            &[],
        )
        .await;
        let error = format!(
            "{:#}",
            result.expect_err("the bogus model must fail the run")
        );
        assert!(
            error.contains("opencode CLI exited with code"),
            "the run must reach OpenCode, not fail to start: {error}"
        );
        let target = CleanupTarget {
            configured_binary: None,
            user_profile: profile,
            scratch_name: String::new(),
            stdout: None,
            seq: 0,
        };
        let prefix = format!("eir-opencode-{}-", std::process::id());
        let mut remaining = usize::MAX;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let listed = run_opencode(&target, &LIST_ARGS)
                .await
                .expect("session list runs");
            remaining = listed.stdout.matches(prefix.as_str()).count();
            if remaining == 0 {
                break;
            }
        }
        assert_eq!(remaining, 0, "the run's session was not deleted");
    }

    /// Real OpenCode check of the fallback: a run whose output was lost is still cleaned up,
    /// found by its scratch directory. `cargo test -p eir-svc -- --ignored real_opencode`
    #[tokio::test]
    #[ignore = "needs the opencode CLI installed for this user"]
    async fn real_opencode_silent_run_is_found_by_directory() {
        let scratch_name = format!("eir-opencode-{}-silent", std::process::id());
        let dir = std::env::temp_dir().join(&scratch_name);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let target = CleanupTarget {
            configured_binary: None,
            user_profile: crate::ai::opencode_cli::resolve_opencode_profile(None),
            scratch_name: scratch_name.clone(),
            stdout: None,
            seq: 0,
        };
        let dir_arg = dir.to_string_lossy().into_owned();
        let run = run_opencode(
            &target,
            &[
                "run",
                "Reply with OK.",
                "--format",
                "json",
                "-m",
                "llamacpp/eir-test-does-not-exist",
                "--dir",
                &dir_arg,
            ],
        )
        .await
        .expect("opencode run starts");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !session_ids_from_ndjson(&run.stdout).is_empty(),
            "the throwaway run must have created a session"
        );
        assert_eq!(cleanup(target).await.expect("cleanup runs"), 1);
        let again = CleanupTarget {
            configured_binary: None,
            user_profile: crate::ai::opencode_cli::resolve_opencode_profile(None),
            scratch_name: scratch_name.clone(),
            stdout: None,
            seq: 0,
        };
        let listed = run_opencode(&again, &LIST_ARGS).await.expect("list runs");
        assert!(session_ids_for_scratch(&listed.stdout, &scratch_name).is_empty());
    }

    #[test]
    fn ids_that_could_carry_shell_syntax_are_refused() {
        let hostile = r#"{"type":"text","sessionID":"ses_x & del /q C:\\*"}"#;
        assert!(session_ids_from_ndjson(hostile).is_empty());
        assert!(!valid_session_id("abc"));
        assert!(!valid_session_id(&format!("ses_{}", "a".repeat(200))));
        assert!(valid_session_id("ses_f2b768d6fffedPbjFS2J9pKby5"));
    }

    #[test]
    fn a_silent_run_is_found_by_its_scratch_directory() {
        let list = r#"[
 {"id":"ses_mine1","directory":"C:\\Users\\Swatto\\AppData\\Local\\Temp\\eir-opencode-15940-87"},
 {"id":"ses_other","directory":"C:\\Users\\Swatto\\AppData\\Local\\Temp\\eir-opencode-15940-870"},
 {"id":"ses_user","directory":"C:\\Users\\Swatto\\eir"},
 {"id":"ses_mine2","directory":"C:/Users/Swatto/AppData/Local/Temp/EIR-OPENCODE-15940-87/"}
]"#;
        assert_eq!(
            session_ids_for_scratch(list, "eir-opencode-15940-87"),
            vec!["ses_mine1", "ses_mine2"]
        );
        assert!(session_ids_for_scratch("not json", "eir-opencode-1-1").is_empty());
        assert!(session_ids_for_scratch("[]", "eir-opencode-1-1").is_empty());
    }
}
