use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api: ApiConfig,
    pub monitoring: MonitoringConfig,
    pub persistence: PersistenceConfig,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiProvider {
    #[default]
    Anthropic,
    OpenAiCompatible,
    /// Spawn the local `claude` CLI binary (no API key required).
    ClaudeCli,
}

#[derive(Debug, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub provider: ApiProvider,
    /// Anthropic native: API key from console.anthropic.com
    pub anthropic_api_key: Option<String>,
    /// OpenAI-compatible proxy: base URL
    pub base_url: Option<String>,
    /// OpenAI-compatible proxy: Bearer token ("not-needed" for claude-max-api-proxy)
    pub api_key: Option<String>,
    /// Model name. Leave empty for claude_cli to use the CLI's configured default.
    #[serde(default)]
    pub model: String,
    /// claude_cli: path to the claude binary. Defaults to "claude" (must be in PATH).
    pub claude_cli_path: Option<String>,
    /// claude_cli: your Windows user profile root (e.g. C:\Users\Swatto).
    /// Required when the service runs as LocalSystem so the CLI can find your login session.
    pub user_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MonitoringConfig {
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub decision_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct PersistenceConfig {
    pub audit_db: String,
}

/// Resolve a path relative to the executable's directory (not cwd).
/// Absolute paths are returned unchanged.
pub fn resolve(rel: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(rel);
    if p.is_absolute() {
        return p;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(rel)))
        .unwrap_or(p)
}

pub fn load(path: &str) -> Result<Config> {
    let resolved = resolve(path);
    let contents = fs::read_to_string(&resolved)
        .with_context(|| format!("Failed to read config file: {}", resolved.display()))?;
    toml::from_str(&contents).with_context(|| "Failed to parse config TOML")
}
