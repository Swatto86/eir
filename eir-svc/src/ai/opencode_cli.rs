//! OpenCode CLI provider — `opencode run --format json` NDJSON (same shape as former Kilo).

use crate::ai::cli_ndjson::parse_agent_ndjson;
use crate::ai::cli_process::{
    char_preview, cli_process, current_user_profile, is_real, validate_cli_model_id, wait_capped,
    CliProcessOutput,
};
use crate::ai::cli_user::running_as_local_system;
use crate::ai::json::sanitize_json;
use crate::models::CallUsage;
use anyhow::{bail, Context, Result};
use tracing::warn;

#[cfg(windows)]
use crate::ai::cli_user::{run_cli_as_active_user, UserCliSpec};

/// Configured profile, else the process USERPROFILE/HOME (OpenCode stores session under it).
pub(crate) fn resolve_opencode_profile(configured: Option<&str>) -> Option<String> {
    if let Some(profile) = configured.filter(|p| is_real(p)) {
        return Some(profile.trim().to_string());
    }
    current_user_profile().map(|p| p.to_string_lossy().into_owned())
}

/// Configured path, else npm shim / common install locations, else bare `opencode`.
pub(crate) fn resolve_opencode_binary(
    configured: Option<&str>,
    user_profile: Option<&str>,
) -> String {
    if let Some(p) = configured.filter(|p| is_real(p)) {
        return p.trim().to_string();
    }
    if let Some(up) = user_profile {
        for candidate in [
            format!("{up}\\.local\\bin\\opencode.exe"),
            format!("{up}\\AppData\\Roaming\\npm\\opencode.cmd"),
            format!("{up}\\AppData\\Roaming\\npm\\opencode.exe"),
        ] {
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    "opencode".into()
}

/// Project-level OpenCode config written into every scratch workspace.
const SCRATCH_CONFIG_NAME: &str = "opencode.json";

/// Files written into the scratch workspace beside the prompt. OpenCode treats `--dir` as
/// the project, so a project-level `opencode.json` there overrides the desktop user's
/// global config: every MCP server the user has enabled for interactive work (Playwright
/// browser, computer use, memory…) is disabled for Eir's unattended run, which otherwise
/// launched a visible automated Chrome each decision cycle. Built-in tools are untouched.
pub(crate) fn opencode_workspace_files(profile: &str) -> Vec<(String, Vec<u8>)> {
    let mut mcp = serde_json::Map::new();
    for name in user_mcp_servers(profile) {
        mcp.insert(name, serde_json::json!({ "enabled": false }));
    }
    let config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": mcp,
    });
    vec![(
        SCRATCH_CONFIG_NAME.to_string(),
        config.to_string().into_bytes(),
    )]
}

/// MCP server names from the user's global OpenCode config (`<profile>/.config/opencode/`).
fn user_mcp_servers(profile: &str) -> Vec<String> {
    let dir = std::path::Path::new(profile)
        .join(".config")
        .join("opencode");
    let mut names = Vec::new();
    for file in ["opencode.json", "opencode.jsonc"] {
        let path = dir.join(file);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Cannot read the user's OpenCode config; its MCP servers stay enabled for this run");
                continue;
            }
        };
        // `.jsonc` (and hand-edited `.json`) may carry comments and trailing commas.
        match serde_json::from_str::<serde_json::Value>(&sanitize_json(&raw)) {
            Ok(config) => names.extend(
                config
                    .get("mcp")
                    .and_then(serde_json::Value::as_object)
                    .into_iter()
                    .flat_map(|servers| servers.keys().cloned()),
            ),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Cannot parse the user's OpenCode config; its MCP servers stay enabled for this run")
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn opencode_variant(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// `opencode run --format json [-m …] [--variant …] [--auto] [--file=…]`.
/// Scratch `--dir` is added by [`UserCliSpec::workspace_flag`] / the local path.
pub(crate) fn build_args(
    model: &str,
    effort: &str,
    web_search: bool,
    image_file_names: &[String],
) -> Result<Vec<String>> {
    validate_cli_model_id(model, "OpenCode")?;
    let mut args = vec!["run".into(), "--format".into(), "json".into()];
    if !model.is_empty() {
        args.extend(["-m".into(), model.into()]);
    }
    if let Some(variant) = opencode_variant(effort) {
        args.extend(["--variant".into(), variant.into()]);
    }
    if web_search {
        args.push("--auto".into());
    }
    for name in image_file_names {
        args.push(format!("--file={name}"));
    }
    Ok(args)
}

pub(crate) async fn call_opencode_cli(
    configured_binary: Option<&str>,
    model: &str,
    effort: &str,
    user_profile: Option<&str>,
    prompt: &str,
    web_search: bool,
    files: &[(String, Vec<u8>)],
) -> Result<(String, Option<CallUsage>)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let args = build_args(model, effort, web_search, &names)?;

    let output = if running_as_local_system() {
        #[cfg(windows)]
        {
            let binary = configured_binary.map(str::to_owned);
            let args = args.clone();
            let prompt = prompt.to_string();
            let files = files.to_vec();
            tokio::task::spawn_blocking(move || {
                run_cli_as_active_user(
                    UserCliSpec {
                        configured_binary: binary.as_deref(),
                        resolve_binary: resolve_opencode_binary,
                        what: "opencode CLI",
                        scratch_prefix: "eir-opencode",
                        workspace_flag: Some("--dir"),
                        workspace_files: opencode_workspace_files,
                        timeout_ms: 300_000,
                    },
                    &args,
                    &prompt,
                    &files,
                    seq,
                )
            })
            .await
            .context("Join OpenCode user-process task")??
        }
        #[cfg(not(windows))]
        {
            let _ = (configured_binary, user_profile, seq, files);
            bail!("opencode LocalSystem launch is Windows-only")
        }
    } else {
        let configured_binary = configured_binary.map(str::to_owned);
        let user_profile = user_profile.map(str::to_owned);
        let profile_for_resolution = user_profile.clone();
        let (binary, extra_files) = tokio::task::spawn_blocking(move || {
            let binary = resolve_opencode_binary(
                configured_binary.as_deref(),
                profile_for_resolution.as_deref(),
            );
            let profile = profile_for_resolution
                .or_else(|| current_user_profile().map(|p| p.to_string_lossy().into_owned()));
            let extra_files = match profile.as_deref() {
                Some(profile) => opencode_workspace_files(profile),
                None => {
                    warn!("No user profile for the opencode CLI; MCP servers cannot be disabled for this run");
                    Vec::new()
                }
            };
            (binary, extra_files)
        })
        .await
        .context("Join OpenCode binary resolution task")?;
        let workspace =
            std::env::temp_dir().join(format!("eir-opencode-{}-{seq}", std::process::id()));
        tokio::fs::create_dir_all(&workspace)
            .await
            .context("Create OpenCode scratch workspace")?;
        for (name, bytes) in files.iter().chain(&extra_files) {
            tokio::fs::write(workspace.join(name), bytes)
                .await
                .with_context(|| format!("Write OpenCode workspace file {name}"))?;
        }
        let mut command = cli_process(&binary);
        command
            .args(&args)
            .arg("--dir")
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
        let child = command.spawn().context(
            "Failed to spawn the opencode CLI — is it installed and logged in on this machine?",
        );
        let waited = match child {
            Ok(child) => wait_capped(child, "opencode CLI", Some(prompt.as_bytes().to_vec())).await,
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
                "opencode CLI exited with code {} and no error output (transient)",
                output.code
            );
        }
        bail!("opencode CLI exited with code {}: {err}", output.code);
    }

    let (text, usage) = parse_agent_ndjson(&output.stdout)?;
    if text.trim().is_empty() {
        bail!("opencode CLI returned no assistant text");
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_web_search_variant_and_files() {
        let args = build_args("ollama/llama3.2", "max", true, &["eir-image-0.png".into()]).unwrap();
        assert_eq!(&args[..3], &["run", "--format", "json"]);
        assert!(args.windows(2).any(|w| w == ["-m", "ollama/llama3.2"]));
        assert!(args.windows(2).any(|w| w == ["--variant", "high"]));
        assert!(args.iter().any(|a| a == "--auto"));
        assert!(args.iter().any(|a| a == "--file=eir-image-0.png"));
        assert!(!args.iter().any(|a| a == "--dir"));
    }

    fn temp_profile(tag: &str) -> std::path::PathBuf {
        let profile =
            std::env::temp_dir().join(format!("eir-oc-profile-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        profile
    }

    #[test]
    fn scratch_config_disables_every_mcp_server_from_the_user_config() {
        let profile = temp_profile("mcp");
        let dir = profile.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission":"allow","mcp":{"browser":{"type":"local","command":["node"],"enabled":true},"memory":{"type":"local","command":["node"]}}}"#,
        )
        .unwrap();
        let files = opencode_workspace_files(&profile.to_string_lossy());
        let _ = std::fs::remove_dir_all(&profile);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "opencode.json");
        let config: serde_json::Value = serde_json::from_slice(&files[0].1).unwrap();
        assert_eq!(config["mcp"]["browser"]["enabled"], false);
        assert_eq!(config["mcp"]["memory"]["enabled"], false);
        assert_eq!(config["mcp"].as_object().unwrap().len(), 2);
        assert!(
            config.get("tools").is_none(),
            "built-in tools must stay available"
        );
        assert!(config.get("permission").is_none());
        // The workspace config is never attached to the prompt.
        assert!(!build_args("m", "", false, &[])
            .unwrap()
            .iter()
            .any(|a| a.contains("opencode.json")));
    }

    #[test]
    fn scratch_config_reads_commented_jsonc_and_merges_both_files() {
        let profile = temp_profile("jsonc");
        let dir = profile.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.jsonc"),
            "{
  \"$schema\": \"https://opencode.ai/config.json\",
  // portable settings
  \"mcp\": {
    /* shared */ \"notes\": { \"type\": \"local\", \"command\": [\"node\"], },
  },
  \"autoupdate\": \"notify\",
}
",
        )
        .unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"mcp":{"browser":{"type":"local","command":["node"]}}}"#,
        )
        .unwrap();
        let files = opencode_workspace_files(&profile.to_string_lossy());
        let _ = std::fs::remove_dir_all(&profile);
        let config: serde_json::Value = serde_json::from_slice(&files[0].1).unwrap();
        assert_eq!(config["mcp"]["notes"]["enabled"], false);
        assert_eq!(config["mcp"]["browser"]["enabled"], false);
        assert_eq!(config["mcp"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn scratch_config_without_user_config_is_valid_json() {
        let profile = temp_profile("none");
        let files = opencode_workspace_files(&profile.to_string_lossy());
        let config: serde_json::Value = serde_json::from_slice(&files[0].1).unwrap();
        assert!(config["mcp"].as_object().unwrap().is_empty());
    }

    #[test]
    fn resolve_binary_prefers_configured() {
        assert_eq!(
            resolve_opencode_binary(Some(r"D:\tools\opencode.exe"), None),
            r"D:\tools\opencode.exe"
        );
        assert_eq!(
            resolve_opencode_binary(Some(r"C:\Users\YourName\opencode.exe"), None),
            "opencode"
        );
    }
}
