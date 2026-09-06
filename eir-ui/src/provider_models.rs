use crate::util::run_command_capped;
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
const OPENCODE_MODELS: &[&str] = &[
    "ollama/llama3.2",
    "opencode/big-pickle",
    "anthropic/claude-sonnet-4-6",
    "openai/gpt-5.4-mini",
];
const CURSOR_MODELS: &[&str] = &[
    "auto",
    "composer-2.5",
    "composer-2.5-fast",
    "gpt-5.6-sol-high",
    "claude-sonnet-5-thinking-high",
];

#[tauri::command]
pub async fn list_provider_models(provider: String) -> Result<Vec<String>, String> {
    match provider.as_str() {
        "claude_cli" => Ok(strings(CLAUDE_MODELS)),
        "codex_cli" => Ok(cli_models("codex", &["debug", "models"])
            .await
            .and_then(|out| parse_codex_models(&out))
            .unwrap_or_else(|| strings(CODEX_MODELS))),
        "opencode_cli" => Ok(cli_models("opencode", &["models"])
            .await
            .and_then(|out| parse_line_models(&out))
            .unwrap_or_else(|| strings(OPENCODE_MODELS))),
        "cursor_cli" => Ok(cli_models("agent", &["--list-models"])
            .await
            .and_then(|out| parse_cursor_models(&out))
            .unwrap_or_else(|| strings(CURSOR_MODELS))),
        _ => Err("Unknown AI provider".into()),
    }
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
            for candidate in [
                dir.join(name),
                dir.join(format!("{name}.exe")),
                dir.join(format!("{name}.cmd")),
            ] {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from(name)
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

fn valid_model_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id.len() <= 200
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':' | '@'))
}

fn parse_line_models(output: &str) -> Option<Vec<String>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for line in output.lines() {
        let id = line.trim();
        if !valid_model_id(id) || !seen.insert(id.to_string()) {
            continue;
        }
        models.push(id.to_string());
    }
    (!models.is_empty()).then_some(models)
}

fn parse_codex_models(output: &str) -> Option<Vec<String>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for line in output.lines() {
        let id = line.split_whitespace().next().unwrap_or("").trim();
        if !valid_model_id(id) || !seen.insert(id.to_string()) {
            continue;
        }
        models.push(id.to_string());
    }
    (!models.is_empty()).then_some(models)
}

/// `agent --list-models` emits `id - Display Name` lines after a header.
fn parse_cursor_models(output: &str) -> Option<Vec<String>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("Available models") {
            continue;
        }
        let id = line.split(" - ").next().unwrap_or(line).trim();
        if !valid_model_id(id) || !seen.insert(id.to_string()) {
            continue;
        }
        models.push(id.to_string());
    }
    (!models.is_empty()).then_some(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_catalog_keeps_provider_slash_ids() {
        let out = "ollama/llama3.2\nopencode/big-pickle\nbad id with spaces\n";
        let models = parse_line_models(out).unwrap();
        assert_eq!(
            models,
            vec![
                "ollama/llama3.2".to_string(),
                "opencode/big-pickle".to_string()
            ]
        );
    }

    #[test]
    fn cursor_catalog_takes_id_before_dash() {
        let out =
            "Available models\n\nauto - Auto (current, default)\ncomposer-2.5 - Composer 2.5\n";
        let models = parse_cursor_models(out).unwrap();
        assert_eq!(models, vec!["auto".to_string(), "composer-2.5".to_string()]);
    }

    #[test]
    fn codex_catalog_takes_first_token() {
        let out = "gpt-5.6-sol  (default)\ngpt-5.4-mini\n";
        let models = parse_codex_models(out).unwrap();
        assert_eq!(
            models,
            vec!["gpt-5.6-sol".to_string(), "gpt-5.4-mini".to_string()]
        );
    }
}
