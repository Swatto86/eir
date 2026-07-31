use crate::util::run_command_capped;
use serde_json::Value;
use std::{collections::HashSet, path::PathBuf, process::Stdio, time::Duration};

const MODEL_OUTPUT_CAP: usize = 2 * 1024 * 1024;

const CLAUDE_MODELS: &[&str] = &[
    "haiku",
    "sonnet",
    "opus",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];
const CODEX_MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];
const KILO_MODELS: &[&str] = &[
    "kilo/x-ai/grok-4.20",
    "kilo/x-ai/grok-4.3",
    "kilo/z-ai/glm-5.2",
    "kilo/z-ai/glm-4.7",
    "kilo/anthropic/claude-opus-4.8",
    "kilo/anthropic/claude-sonnet-5",
    "kilo/openai/gpt-5.6-sol",
    "kilo/google/gemini-3.1-pro-preview",
    "kilo/deepseek/deepseek-v4-pro",
    "kilo/moonshotai/kimi-k2.7-code",
    "kilo/minimax/minimax-m3",
];
const OPENROUTER_MODELS: &[&str] = &[
    "openrouter/free",
    "openrouter/auto",
    "openai/gpt-5.4-mini",
    "anthropic/claude-sonnet-5",
    "google/gemini-3.1-pro-preview",
    "x-ai/grok-4.20",
    "deepseek/deepseek-v4-pro",
    "minimax/minimax-m3",
];

#[tauri::command]
pub async fn list_provider_models(provider: String) -> Result<Vec<String>, String> {
    match provider.as_str() {
        "anthropic" | "claude_cli" => Ok(strings(CLAUDE_MODELS)),
        "codex_cli" => Ok(cli_models("codex", &["debug", "models"])
            .await
            .and_then(|out| parse_codex_models(&out))
            .unwrap_or_else(|| strings(CODEX_MODELS))),
        "kilo_cli" => Ok(cli_models("kilo", &["models", "--pure"])
            .await
            .and_then(|out| parse_line_models(&out))
            .unwrap_or_else(|| strings(KILO_MODELS))),
        "openrouter" => Ok(openrouter_models()
            .await
            .unwrap_or_else(|| strings(OPENROUTER_MODELS))),
        _ => Err("Unknown AI provider".into()),
    }
}

async fn openrouter_models() -> Option<Vec<String>> {
    let mut command = tokio::process::Command::new("powershell.exe");
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let (status, stdout, _) = run_command_capped(
        command
            .args([
                "-NoProfile",
                "-Command",
                "try { @((Invoke-RestMethod -Uri 'https://openrouter.ai/api/v1/models' -TimeoutSec 15).data.id) | ConvertTo-Json -Compress } catch { '' }",
            ])
            .stdin(Stdio::null()),
        Duration::from_secs(20),
        MODEL_OUTPUT_CAP,
    )
    .await?;
    if !status.success() {
        return None;
    }
    let mut models: Vec<String> = serde_json::from_slice::<Vec<String>>(&stdout)
        .ok()?
        .into_iter()
        .filter(|id| valid_model_id(id))
        .collect();
    models.sort_unstable();
    models.dedup();
    if !models.iter().any(|model| model == "openrouter/free") {
        models.insert(0, "openrouter/free".into());
    }
    (!models.is_empty()).then_some(models)
}

async fn cli_models(binary: &str, args: &[&str]) -> Option<String> {
    let binary = resolve_binary(binary);
    let lower = binary.to_string_lossy().to_ascii_lowercase();
    let mut command = if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.arg("/C").arg(&binary);
        command
    } else {
        tokio::process::Command::new(&binary)
    };
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let (status, stdout, _) = run_command_capped(
        command.args(args).stdin(Stdio::null()),
        Duration::from_secs(30),
        MODEL_OUTPUT_CAP,
    )
    .await?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&stdout).into_owned())
}

fn resolve_binary(name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{name}.{extension}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    if let Some(profile) = profile {
        let candidates = match name {
            "codex" => vec![
                profile.join("AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe"),
                profile.join(".codex\\packages\\standalone\\current\\bin\\codex.exe"),
                profile.join("AppData\\Roaming\\npm\\codex.cmd"),
            ],
            "kilo" => vec![profile.join("AppData\\Roaming\\npm\\kilo.cmd")],
            _ => Vec::new(),
        };
        if let Some(candidate) = candidates.into_iter().find(|path| path.is_file()) {
            return candidate;
        }
    }
    PathBuf::from(name)
}

fn parse_codex_models(output: &str) -> Option<Vec<String>> {
    let root: Value = serde_json::from_str(output).ok()?;
    let models: Vec<String> = root["models"]
        .as_array()?
        .iter()
        .filter(|model| model["visibility"].as_str() == Some("list"))
        .filter_map(|model| model["slug"].as_str())
        .filter(|id| valid_model_id(id))
        .map(str::to_string)
        .collect();
    (!models.is_empty()).then_some(models)
}

fn parse_line_models(output: &str) -> Option<Vec<String>> {
    let mut seen = HashSet::new();
    let models: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|id| valid_model_id(id))
        .map(str::to_string)
        .filter(|id| seen.insert(id.clone()))
        .collect();
    (!models.is_empty()).then_some(models)
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@~,+=-".contains(c))
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_catalog_filters_hidden_models() {
        let models = parse_codex_models(
            r#"{"models":[
                {"slug":"gpt-5.6-sol","visibility":"list"},
                {"slug":"codex-auto-review","visibility":"hide"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(models, ["gpt-5.6-sol"]);
    }

    #[test]
    fn line_catalog_rejects_logs_and_shell_characters() {
        let models = parse_line_models("kilo/openai/gpt-5.6-sol\nlog line\nbad&id\n").unwrap();
        assert_eq!(models, ["kilo/openai/gpt-5.6-sol"]);
    }

    #[test]
    fn line_catalog_removes_non_adjacent_duplicates() {
        let models = parse_line_models("kilo/a\nkilo/b\nkilo/a\n").unwrap();
        assert_eq!(models, ["kilo/a", "kilo/b"]);
    }
}
