use anyhow::{Context, Result};
use sentry_proto::{SettingsUpdate, UiSettings};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub monitoring: MonitoringConfig,
    pub persistence: PersistenceConfig,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy)]
pub enum ApiProvider {
    #[default]
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_compatible", alias = "open_ai_compatible")]
    OpenAiCompatible,
    /// OpenRouter (openrouter.ai) — OpenAI-compatible; supports free models.
    #[serde(rename = "openrouter", alias = "open_router")]
    OpenRouter,
    /// Spawn the local `claude` CLI binary (no API key required).
    #[serde(rename = "claude_cli")]
    ClaudeCli,
}

impl ApiProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiProvider::Anthropic => "anthropic",
            ApiProvider::OpenAiCompatible => "openai_compatible",
            ApiProvider::OpenRouter => "openrouter",
            ApiProvider::ClaudeCli => "claude_cli",
        }
    }

    fn parse(s: &str) -> ApiProvider {
        match s {
            "anthropic" => ApiProvider::Anthropic,
            "openai_compatible" => ApiProvider::OpenAiCompatible,
            "openrouter" => ApiProvider::OpenRouter,
            _ => ApiProvider::ClaudeCli,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub provider: ApiProvider,
    /// Anthropic native: API key from console.anthropic.com
    pub anthropic_api_key: Option<String>,
    /// OpenAI-compatible proxy: base URL
    pub base_url: Option<String>,
    /// OpenAI-compatible proxy: Bearer token ("not-needed" for claude-max-api-proxy)
    pub api_key: Option<String>,
    /// OpenRouter API key (provider = "openrouter").
    pub openrouter_api_key: Option<String>,
    /// Model name. Leave empty for claude_cli to use the CLI's configured default.
    #[serde(default)]
    pub model: String,
    /// Claude model for the on-demand app-update check (empty = CLI default).
    #[serde(default)]
    pub update_check_model: String,
    /// claude_cli: path to the claude binary. Defaults to "claude" (must be in PATH).
    pub claude_cli_path: Option<String>,
    /// claude_cli: your Windows user profile root (e.g. C:\Users\Swatto).
    /// Required when the service runs as LocalSystem so the CLI can find your login session.
    pub user_profile: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MonitoringConfig {
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub decision_interval_secs: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PersistenceConfig {
    pub audit_db: String,
}

impl Config {
    /// Current settings for the UI (no secrets — only whether they are set).
    pub fn to_ui_settings(&self) -> UiSettings {
        let set = |o: &Option<String>| o.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        UiSettings {
            provider: self.api.provider.as_str().to_string(),
            model: self.api.model.clone(),
            update_check_model: self.api.update_check_model.clone(),
            base_url: self.api.base_url.clone().unwrap_or_default(),
            decision_interval_secs: self.monitoring.decision_interval_secs,
            event_log_poll_interval_secs: self.monitoring.event_log_poll_interval_secs,
            wmi_poll_interval_secs: self.monitoring.wmi_poll_interval_secs,
            event_log_channels: self.monitoring.event_log_channels.clone(),
            log_directories: self.monitoring.log_directories.clone(),
            openrouter_key_set: set(&self.api.openrouter_api_key),
            anthropic_key_set: set(&self.api.anthropic_api_key),
            api_key_set: set(&self.api.api_key),
        }
    }

    /// Apply an update from the UI. Empty/None secret fields keep the stored value.
    pub fn apply_update(&mut self, u: SettingsUpdate) {
        self.api.provider = ApiProvider::parse(&u.provider);
        self.api.model = u.model;
        self.api.update_check_model = u.update_check_model;
        let keep = |cur: &mut Option<String>, new: Option<String>| {
            if let Some(v) = new {
                if !v.trim().is_empty() {
                    *cur = Some(v);
                }
            }
        };
        if let Some(b) = u.base_url {
            self.api.base_url = if b.trim().is_empty() { None } else { Some(b) };
        }
        keep(&mut self.api.openrouter_api_key, u.openrouter_api_key);
        keep(&mut self.api.anthropic_api_key, u.anthropic_api_key);
        keep(&mut self.api.api_key, u.api_key);
        self.monitoring.decision_interval_secs = u.decision_interval_secs.max(10);
        self.monitoring.event_log_poll_interval_secs = u.event_log_poll_interval_secs.max(5);
        self.monitoring.wmi_poll_interval_secs = u.wmi_poll_interval_secs.max(30);
        self.monitoring.event_log_channels = u.event_log_channels;
        self.monitoring.log_directories = u.log_directories;
    }
}

/// Write the config back to disk (resolved relative to the exe directory).
pub fn save(config: &Config, path: &str) -> Result<()> {
    let resolved = resolve(path);
    let toml = toml::to_string_pretty(config).context("Failed to serialize config")?;
    fs::write(&resolved, toml)
        .with_context(|| format!("Failed to write config file: {}", resolved.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[api]
provider = "claude_cli"
model = ""
[monitoring]
event_log_channels = ["System"]
log_directories = []
event_log_poll_interval_secs = 30
wmi_poll_interval_secs = 300
decision_interval_secs = 600
[persistence]
audit_db = "./sentry.db"
"#;

    #[test]
    fn apply_update_then_toml_round_trips() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "openrouter".into(),
            model: "nvidia/nemotron-3-super-120b-a12b:free".into(),
            update_check_model: "claude-haiku-4-5".into(),
            base_url: Some(String::new()),
            openrouter_api_key: Some("sk-or-test".into()),
            anthropic_api_key: None,
            api_key: None,
            decision_interval_secs: 900,
            event_log_poll_interval_secs: 45,
            wmi_poll_interval_secs: 300,
            event_log_channels: vec!["System".into(), "Application".into()],
            log_directories: vec!["C:\\Logs".into()],
        });
        // Must serialize to TOML the loader can read back (else a settings save bricks the service).
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider.as_str(), "openrouter");
        assert_eq!(reparsed.api.model, "nvidia/nemotron-3-super-120b-a12b:free");
        assert_eq!(
            reparsed.api.openrouter_api_key.as_deref(),
            Some("sk-or-test")
        );
        assert_eq!(reparsed.monitoring.decision_interval_secs, 900);
        assert_eq!(reparsed.api.update_check_model, "claude-haiku-4-5");
        assert_eq!(reparsed.monitoring.event_log_channels.len(), 2);
        // Blank api key keeps the prior value (None here).
        assert!(reparsed.api.anthropic_api_key.is_none());
    }

    #[test]
    fn legacy_open_router_provider_alias_still_parses() {
        // Older configs serialized OpenRouter as "open_router"; must still load.
        let toml = SAMPLE.replace("\"claude_cli\"", "\"open_router\"");
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.api.provider.as_str(), "openrouter");
    }
}
