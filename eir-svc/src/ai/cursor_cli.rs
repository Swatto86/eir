//! Cursor Agent CLI provider — binary is `agent` (shim `~/.local/bin/agent.cmd`).
//! Text-only: Cursor's ask mode has no image-attach flag (Codex/OpenCode do).

use crate::ai::cli_process::{
    char_preview, cli_process, current_user_profile, is_real, validate_cli_model_id, wait_capped,
    CliProcessOutput,
};
use crate::ai::cli_user::running_as_local_system;
use crate::models::CallUsage;
use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[cfg(windows)]
use crate::ai::cli_user::{run_cli_as_active_user, UserCliSpec};

pub(crate) fn resolve_cursor_profile(configured: Option<&str>) -> Option<String> {
    if let Some(profile) = configured.filter(|p| is_real(p)) {
        return Some(profile.trim().to_string());
    }
    current_user_profile().map(|p| p.to_string_lossy().into_owned())
}

/// Prefer `agent.cmd` / `agent.exe` under `.local\bin`, else bare `agent`.
pub(crate) fn resolve_cursor_binary(
    configured: Option<&str>,
    user_profile: Option<&str>,
) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\.local\\bin\\agent.cmd"),
            format!("{up}\\.local\\bin\\agent.exe"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "agent".into()
}

pub(crate) fn build_args(model: &str) -> Result<Vec<String>> {
    validate_cli_model_id(model, "Cursor")?;
    let mut args = vec![
        "-p".into(),
        "--mode".into(),
        "ask".into(),
        "--output-format".into(),
        "json".into(),
        "--trust".into(),
    ];
    if !model.is_empty() {
        args.extend(["--model".into(), model.into()]);
    }
    Ok(args)
}

#[derive(Deserialize)]
struct CursorResult {
    #[serde(rename = "type")]
    kind: Option<String>,
    result: Option<String>,
    usage: Option<CursorUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

/// Parse the single JSON envelope `{type:result, result, usage:{…}}`.
pub(crate) fn parse_cursor_json(stdout: &str) -> Result<(String, Option<CallUsage>)> {
    let trimmed = stdout.trim();
    // Prefer a line that looks like the result envelope; else whole stdout.
    let candidate = trimmed
        .lines()
        .rev()
        .find(|l| l.trim().starts_with('{') && l.contains("\"result\""))
        .unwrap_or(trimmed);
    let env: CursorResult =
        serde_json::from_str(candidate).context("cursor CLI returned non-JSON output")?;
    if let Some(kind) = env.kind.as_deref() {
        if kind != "result" {
            bail!("cursor CLI returned unexpected type: {kind}");
        }
    }
    let text = env.result.unwrap_or_default();
    if text.trim().is_empty() {
        bail!("cursor CLI returned an empty result");
    }
    let usage = env.usage.map(|u| CallUsage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_creation: u.cache_write_tokens.unwrap_or(0),
        cache_read: u.cache_read_tokens.unwrap_or(0),
        cost_usd: 0.0,
    });
    Ok((text, usage))
}

pub(crate) async fn call_cursor_cli(
    configured_binary: Option<&str>,
    model: &str,
    user_profile: Option<&str>,
    prompt: &str,
) -> Result<(String, Option<CallUsage>)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = build_args(model)?;

    let output = if running_as_local_system() {
        #[cfg(windows)]
        {
            let binary = configured_binary.map(str::to_owned);
            let args = args.clone();
            let prompt = prompt.to_string();
            tokio::task::spawn_blocking(move || {
                run_cli_as_active_user(
                    UserCliSpec {
                        configured_binary: binary.as_deref(),
                        resolve_binary: resolve_cursor_binary,
                        what: "cursor CLI",
                        scratch_prefix: "eir-cursor",
                        workspace_flag: Some("--workspace"),
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    &[],
                    seq,
                )
            })
            .await
            .context("Join Cursor user-process task")??
        }
        #[cfg(not(windows))]
        {
            let _ = (configured_binary, user_profile, seq);
            bail!("cursor LocalSystem launch is Windows-only")
        }
    } else {
        let configured_binary = configured_binary.map(str::to_owned);
        let user_profile = user_profile.map(str::to_owned);
        let profile_for_resolution = user_profile.clone();
        let binary = tokio::task::spawn_blocking(move || {
            resolve_cursor_binary(
                configured_binary.as_deref(),
                profile_for_resolution.as_deref(),
            )
        })
        .await
        .context("Join Cursor binary resolution task")?;
        let workspace =
            std::env::temp_dir().join(format!("eir-cursor-{}-{seq}", std::process::id()));
        tokio::fs::create_dir_all(&workspace)
            .await
            .context("Create Cursor scratch workspace")?;
        let mut command = cli_process(&binary);
        command
            .args(&args)
            .arg("--workspace")
            .arg(&workspace)
            .current_dir(&workspace)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(profile) = user_profile.as_deref() {
            command
                .env("USERPROFILE", profile)
                .env("HOME", profile)
                .env("APPDATA", format!("{profile}\\AppData\\Roaming"))
                .env("LOCALAPPDATA", format!("{profile}\\AppData\\Local"));
        }
        let child = command
            .spawn()
            .context("Failed to spawn the cursor agent CLI — is `agent` installed and logged in?");
        let waited = match child {
            Ok(child) => wait_capped(child, "cursor CLI", Some(prompt.as_bytes().to_vec())).await,
            Err(error) => Err(error),
        };
        let _ = tokio::fs::remove_dir_all(&workspace).await;
        let (status, stdout, stderr) = waited?;
        CliProcessOutput {
            code: status.code().map(|code| code as u32).unwrap_or(u32::MAX),
            stdout,
            stderr,
        }
    };

    if output.code != 0 {
        let err = char_preview(output.stderr.trim(), 2000);
        if err.is_empty() {
            bail!(
                "cursor CLI exited with code {} and no error output (transient)",
                output.code
            );
        }
        bail!("cursor CLI exited with code {}: {err}", output.code);
    }
    parse_cursor_json(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cursor_json_reads_result_and_usage() {
        let raw = r#"{"type":"result","result":"hello","usage":{"inputTokens":10,"outputTokens":3,"cacheReadTokens":1,"cacheWriteTokens":2}}"#;
        let (text, usage) = parse_cursor_json(raw).unwrap();
        assert_eq!(text, "hello");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cache_read, 1);
        assert_eq!(u.cache_creation, 2);
    }

    #[test]
    fn build_args_include_model() {
        let args = build_args("gpt-5").unwrap();
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5"]));
        assert!(args.iter().any(|a| a == "--trust"));
        assert!(build_args("bad model!").is_err());
    }
}
