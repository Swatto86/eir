//! Codex CLI provider — `codex exec --json` with optional `--search` and `--image=`.

use crate::ai::cli_process::{
    char_preview, cli_process, is_real, validate_cli_model_id, wait_capped, CliProcessOutput,
};
use crate::ai::cli_user::running_as_local_system;
use crate::models::CallUsage;
use anyhow::{bail, Context, Result};
use serde_json::Value;

#[cfg(windows)]
use crate::ai::cli_user::{run_cli_as_active_user, UserCliSpec};

pub(crate) async fn call_codex_cli(
    configured_binary: Option<&str>,
    model: &str,
    effort: &str,
    prompt: &str,
    web_search: bool,
    files: &[(String, Vec<u8>)],
) -> Result<(String, Option<CallUsage>)> {
    static CODEX_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = CODEX_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    let args = codex_cli_args(model, effort, web_search, &names)?;
    let output = run_codex_cli(configured_binary, &args, prompt, files, seq).await?;

    if output.code != 0 {
        if let Some(err) = codex_event_error(&output.stdout) {
            bail!("codex CLI exited with code {}: {err}", output.code);
        }
        let err = char_preview(output.stderr.trim(), 2000);
        if err.is_empty() {
            bail!(
                "codex CLI exited with code {} and no error output (transient)",
                output.code
            );
        }
        bail!("codex CLI exited with code {}: {err}", output.code);
    }

    parse_codex_ndjson(&output.stdout)
}

fn codex_cli_args(
    model: &str,
    effort: &str,
    web_search: bool,
    image_files: &[String],
) -> Result<Vec<String>> {
    let model = model.trim();
    validate_cli_model_id(model, "Codex")?;
    let mut args = Vec::new();
    if web_search {
        args.push("--search".into());
    }
    args.extend(
        [
            "--ask-for-approval",
            "never",
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "never",
        ]
        .into_iter()
        .map(str::to_string),
    );
    if !model.is_empty() {
        args.extend(["-m".into(), model.into()]);
    }
    if let Some(level) = codex_cli_effort(model, effort) {
        args.extend(["-c".into(), format!("model_reasoning_effort={level}")]);
    }
    for name in image_files {
        args.push(format!("--image={name}"));
    }
    args.push("-".into());
    Ok(args)
}

fn codex_cli_effort<'a>(model: &str, effort: &'a str) -> Option<&'a str> {
    match effort {
        "low" | "medium" | "high" | "xhigh" => Some(effort),
        "max" if model.starts_with("gpt-5.6") => Some("max"),
        "max" => Some("xhigh"),
        _ => None,
    }
}

fn resolve_codex_binary(configured: Option<&str>, user_profile: Option<&str>) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe"),
            format!("{up}\\.codex\\packages\\standalone\\current\\bin\\codex.exe"),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\bin\\codex.exe"
            ),
            format!(
                "{up}\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\codex\\codex.exe"
            ),
            format!("{up}\\AppData\\Roaming\\npm\\codex.cmd"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "codex".into()
}

async fn run_codex_cli(
    configured_binary: Option<&str>,
    args: &[String],
    prompt: &str,
    files: &[(String, Vec<u8>)],
    seq: u64,
) -> Result<CliProcessOutput> {
    if running_as_local_system() {
        #[cfg(windows)]
        {
            let binary = configured_binary.map(str::to_owned);
            let args = args.to_vec();
            let prompt = prompt.to_owned();
            let files = files.to_vec();
            return tokio::task::spawn_blocking(move || {
                run_cli_as_active_user(
                    UserCliSpec {
                        configured_binary: binary.as_deref(),
                        resolve_binary: resolve_codex_binary,
                        what: "codex CLI",
                        scratch_prefix: "eir-codex",
                        workspace_flag: None,
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    &files,
                    seq,
                )
            })
            .await
            .context("Join Codex user-process task")?;
        }
        #[cfg(not(windows))]
        {
            let _ = (configured_binary, args, prompt, files, seq);
            bail!("codex LocalSystem launch is Windows-only");
        }
    }

    let configured_binary = configured_binary.map(str::to_owned);
    let profile = std::env::var("USERPROFILE").ok();
    let binary = tokio::task::spawn_blocking(move || {
        resolve_codex_binary(configured_binary.as_deref(), profile.as_deref())
    })
    .await
    .context("Join Codex binary resolution task")?;
    let workspace = std::env::temp_dir().join(format!("eir-codex-{}-{seq}", std::process::id()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .context("Create Codex CLI scratch workspace")?;
    for (name, bytes) in files {
        tokio::fs::write(workspace.join(name), bytes)
            .await
            .context("Write Codex image attachment")?;
    }
    let mut command = cli_process(&binary);
    command
        .args(args)
        .current_dir(&workspace)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command.spawn().context(
        "Failed to spawn the codex CLI — is it installed and logged in (`codex login`) on this machine?",
    );
    let waited = match child {
        Ok(child) => wait_capped(child, "codex CLI", Some(prompt.as_bytes().to_vec())).await,
        Err(error) => Err(error),
    };
    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let (status, stdout, stderr) = waited?;
    Ok(CliProcessOutput {
        code: status.code().map(|code| code as u32).unwrap_or(u32::MAX),
        stdout,
        stderr,
    })
}

fn codex_event_error(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let event = serde_json::from_str::<Value>(line).ok()?;
        match event["type"].as_str()? {
            "turn.failed" => event["error"]["message"]
                .as_str()
                .map(|s| char_preview(s, 2000)),
            "error" => event["message"].as_str().map(|s| char_preview(s, 2000)),
            _ => None,
        }
    })
}

fn parse_codex_ndjson(stdout: &str) -> Result<(String, Option<CallUsage>)> {
    if let Some(error) = codex_event_error(stdout) {
        bail!("codex CLI: {error}");
    }
    let mut text = String::new();
    let mut usage = None;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("item.completed") if event["item"]["type"].as_str() == Some("agent_message") => {
                if let Some(part) = event["item"]["text"].as_str().filter(|s| !s.is_empty()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            Some("turn.completed") => {
                let u = &event["usage"];
                let cached = u["cached_input_tokens"].as_u64().unwrap_or(0);
                usage = Some(CallUsage {
                    input_tokens: u["input_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(cached),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_creation: u["cache_write_input_tokens"].as_u64().unwrap_or(0),
                    cache_read: cached,
                    cost_usd: 0.0,
                });
            }
            _ => {}
        }
    }
    if text.trim().is_empty() {
        bail!("codex CLI returned no assistant text");
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_cli_args_are_safe_and_provider_appropriate() {
        let args = codex_cli_args("gpt-5.6-sol", "max", true, &[]).unwrap();
        assert_eq!(args.first().map(String::as_str), Some("--search"));
        assert!(args.windows(2).any(|w| w == ["-m", "gpt-5.6-sol"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=max"]));
        assert_eq!(args.last().map(String::as_str), Some("-"));

        let with_images = codex_cli_args(
            "gpt-5.6-sol",
            "high",
            false,
            &["eir-image-0.jpg".to_string(), "eir-image-1.png".to_string()],
        )
        .unwrap();
        assert_eq!(with_images.last().map(String::as_str), Some("-"));
        assert_eq!(
            with_images
                .iter()
                .filter(|a| a.starts_with("--image="))
                .collect::<Vec<_>>(),
            vec!["--image=eir-image-0.jpg", "--image=eir-image-1.png"]
        );

        let older = codex_cli_args("gpt-5.4", "max", false, &[]).unwrap();
        assert!(older
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=xhigh"]));
        assert!(codex_cli_args("gpt-5.4 & whoami", "high", false, &[]).is_err());
        assert!(codex_cli_args(&"a".repeat(257), "high", false, &[]).is_err());
    }

    #[test]
    fn codex_ndjson_parses_text_usage_and_errors() {
        let stream = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"id\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"hello\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,",
            "\"cached_input_tokens\":40,\"cache_write_input_tokens\":3,\"output_tokens\":12}}\n"
        );
        let (text, usage) = parse_codex_ndjson(stream).unwrap();
        assert_eq!(text, "hello");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 60);
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.cache_creation, 3);
        assert_eq!(usage.output_tokens, 12);

        let failed = r#"{"type":"turn.failed","error":{"message":"login required"}}"#;
        assert!(parse_codex_ndjson(failed)
            .unwrap_err()
            .to_string()
            .contains("login required"));
    }

    #[test]
    fn codex_binary_resolution_prefers_real_installs() {
        assert_eq!(
            resolve_codex_binary(Some("D:\\tools\\codex.exe"), None),
            "D:\\tools\\codex.exe"
        );
        let root = std::env::temp_dir().join(format!("eir-codex-resolve-{}", std::process::id()));
        let profile = root.to_string_lossy().into_owned();
        assert_eq!(resolve_codex_binary(None, Some(&profile)), "codex");
        let standalone = root.join(".codex\\packages\\standalone\\current\\bin\\codex.exe");
        std::fs::create_dir_all(standalone.parent().unwrap()).unwrap();
        std::fs::write(&standalone, b"x").unwrap();
        assert_eq!(
            resolve_codex_binary(None, Some(&profile)),
            standalone.to_string_lossy()
        );
        let app = root.join("AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&app, b"x").unwrap();
        assert_eq!(
            resolve_codex_binary(None, Some(&profile)),
            app.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
