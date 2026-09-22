//! Claude Code CLI provider — `claude --print --output-format json`.

use crate::ai::cli_ndjson::provider_cost;
use crate::ai::cli_process::{
    char_preview, cli_process, current_user_profile, is_real, resolve_profile_with_marker,
    validate_cli_model_id, wait_capped, CliProcessOutput,
};
use crate::ai::cli_user::running_as_local_system;
use crate::models::CallUsage;
use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[cfg(windows)]
use crate::ai::cli_user::{run_cli_as_active_user, UserCliSpec};

#[derive(Deserialize)]
struct ClaudeCliResult {
    result: Option<String>,
    total_cost_usd: Option<f64>,
    usage: Option<ClaudeCliUsage>,
}

#[derive(Deserialize)]
struct ClaudeCliUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// Configured profile, else process USERPROFILE/HOME when `.claude/.credentials.json` exists.
pub(crate) fn resolve_claude_profile(configured: Option<&str>) -> Option<String> {
    resolve_profile_with_marker(
        configured,
        current_user_profile(),
        &[".claude", ".credentials.json"],
    )
}

/// Prefer native `.local\bin`, npm bundle, shim, else bare `claude`.
pub(crate) fn resolve_claude_binary(
    configured: Option<&str>,
    user_profile: Option<&str>,
) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\.local\\bin\\claude.exe"),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe"
            ),
            format!("{up}\\AppData\\Roaming\\npm\\claude.cmd"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "claude".into()
}

pub(crate) fn claude_cli_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_lowercase();
    let is_claude =
        matches!(lower.as_str(), "haiku" | "sonnet" | "opus") || lower.starts_with("claude");
    if is_claude {
        m.to_string()
    } else {
        "haiku".to_string()
    }
}

fn build_args(model: &str, effort: &str) -> Result<Vec<String>> {
    validate_cli_model_id(model, "Claude")?;
    let mut args = vec!["--print".into(), "--output-format".into(), "json".into()];
    if !model.is_empty() {
        args.extend(["--model".into(), model.into()]);
    }
    if !effort.is_empty() {
        args.extend(["--effort".into(), effort.into()]);
    }
    Ok(args)
}

pub(crate) fn parse_claude_json(stdout: &str) -> Result<(String, Option<CallUsage>)> {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        bail!("claude CLI returned empty output");
    }
    match serde_json::from_str::<ClaudeCliResult>(stdout) {
        Ok(env) => {
            let text = env.result.unwrap_or_default();
            if text.trim().is_empty() {
                bail!("claude CLI returned an empty result");
            }
            let usage = env.usage.map(|u| CallUsage {
                input_tokens: u.input_tokens.unwrap_or(0),
                output_tokens: u.output_tokens.unwrap_or(0),
                cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                cost_usd: provider_cost(env.total_cost_usd.unwrap_or(0.0)),
            });
            Ok((text, usage))
        }
        Err(_) => Ok((stdout.to_string(), None)),
    }
}

pub(crate) async fn call_claude_cli(
    configured_binary: Option<&str>,
    model: &str,
    effort: &str,
    user_profile: Option<&str>,
    prompt: &str,
) -> Result<(String, Option<CallUsage>)> {
    static CLAUDE_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = CLAUDE_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = build_args(model, effort)?;

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
                        resolve_binary: resolve_claude_binary,
                        what: "claude CLI",
                        scratch_prefix: "eir-claude",
                        workspace_flag: None,
                        workspace_files: |_| Vec::new(),
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    &[],
                    seq,
                )
            })
            .await
            .context("Join Claude user-process task")??
        }
        #[cfg(not(windows))]
        {
            let _ = seq;
            bail!("claude LocalSystem launch is Windows-only")
        }
    } else {
        let configured_binary = configured_binary.map(str::to_owned);
        let user_profile = user_profile.map(str::to_owned);
        let profile_for_resolution = user_profile.clone();
        let binary = tokio::task::spawn_blocking(move || {
            resolve_claude_binary(
                configured_binary.as_deref(),
                profile_for_resolution.as_deref(),
            )
        })
        .await
        .context("Join Claude binary resolution task")?;
        let mut command = cli_process(&binary);
        command
            .args(&args)
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
        let child = command.spawn().context(
            "Failed to spawn the claude CLI — is it installed and logged in on this machine?",
        )?;
        let (status, stdout, stderr) =
            wait_capped(child, "claude CLI", Some(prompt.as_bytes().to_vec())).await?;
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
                "claude CLI exited with code {} and no error output (transient)",
                output.code
            );
        }
        bail!("claude CLI exited with code {}: {err}", output.code);
    }

    parse_claude_json(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_cli_model_coercion() {
        assert_eq!(claude_cli_model(""), "haiku");
        assert_eq!(claude_cli_model("haiku"), "haiku");
        assert_eq!(claude_cli_model("opus"), "opus");
        assert_eq!(claude_cli_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(claude_cli_model("openrouter/free"), "haiku");
    }

    #[test]
    fn claude_binary_resolution_prefers_real_installs() {
        assert_eq!(
            resolve_claude_binary(Some("D:\\tools\\claude.exe"), None),
            "D:\\tools\\claude.exe"
        );
        assert_eq!(
            resolve_claude_binary(Some("C:\\Users\\YourName\\claude.exe"), None),
            "claude"
        );
        let root = std::env::temp_dir().join(format!("eir-claude-resolve-{}", std::process::id()));
        let up = root.to_string_lossy().into_owned();
        assert_eq!(resolve_claude_binary(None, Some(&up)), "claude");
        let npm_exe = root.join(
            "AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe",
        );
        std::fs::create_dir_all(npm_exe.parent().unwrap()).unwrap();
        std::fs::write(&npm_exe, b"x").unwrap();
        assert_eq!(
            resolve_claude_binary(None, Some(&up)),
            npm_exe.to_string_lossy()
        );
        let native = root.join(".local\\bin\\claude.exe");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, b"x").unwrap();
        assert_eq!(
            resolve_claude_binary(None, Some(&up)),
            native.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_cli_envelope_parses_usage() {
        let raw = r#"{"result":"{\"analysis\":\"ok\"}","total_cost_usd":0.0123,
            "usage":{"input_tokens":100,"output_tokens":20,
            "cache_creation_input_tokens":5,"cache_read_input_tokens":50}}"#;
        let (text, usage) = parse_claude_json(raw).unwrap();
        assert_eq!(text, "{\"analysis\":\"ok\"}");
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cache_read, 50);
        assert_eq!(u.cost_usd, 0.0123);
        let (sparse_text, sparse_usage) = parse_claude_json(r#"{"result":"hi"}"#).unwrap();
        assert_eq!(sparse_text, "hi");
        assert!(sparse_usage.is_none());
    }
}
