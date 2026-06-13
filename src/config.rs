use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api: ApiConfig,
    pub monitoring: MonitoringConfig,
    pub persistence: PersistenceConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Deserialize)]
pub struct ApiConfig {
    pub anthropic_api_key: String,
    pub model: String,
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

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

pub fn load(path: &str) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    toml::from_str(&contents).with_context(|| "Failed to parse config TOML")
}
