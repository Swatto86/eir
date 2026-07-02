use crate::updater::config::UpdaterConfig;
use anyhow::{Context, Result};
use eir_proto::{SettingsUpdate, UiSettings};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub monitoring: MonitoringConfig,
    pub persistence: PersistenceConfig,
    /// Autonomous app-update settings. `#[serde(default)]` so a `config.toml`
    /// written before the updater existed (no `[updater]` section) still loads.
    #[serde(default)]
    pub updater: UpdaterConfig,
    /// Advisor-mode settings (AI self-tunes reasoning effort/model). `#[serde(default)]`
    /// so a `config.toml` without an `[advisor]` section still loads.
    #[serde(default)]
    pub advisor: AdvisorConfig,
}

/// When the AI flags a hard/ambiguous situation (or its confidence is low), Eir can
/// re-analyse once at a higher reasoning effort and/or a stronger model — bounded by
/// a daily spend cap. Off by default; the escalation tier is fixed config (never
/// AI-chosen).
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct AdvisorConfig {
    pub enabled: bool,
    /// Stronger model to switch to for the deeper pass (empty = keep the base model).
    pub escalation_model: String,
    /// Higher reasoning effort for the deeper pass (empty = keep base).
    pub escalation_effort: String,
    /// Escalate when the best reported confidence is below this (0.0–1.0).
    pub low_confidence_threshold: f32,
    /// Cap on escalation AI spend per day (USD); 0 = no explicit cap.
    pub budget_usd_per_day: f64,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            escalation_model: String::new(),
            escalation_effort: String::new(),
            low_confidence_threshold: 0.6,
            budget_usd_per_day: 0.50,
        }
    }
}

impl AdvisorConfig {
    pub fn to_view(&self) -> eir_proto::AdvisorSettingsView {
        eir_proto::AdvisorSettingsView {
            enabled: self.enabled,
            escalation_model: self.escalation_model.clone(),
            escalation_effort: self.escalation_effort.clone(),
            low_confidence_threshold: self.low_confidence_threshold,
            budget_usd_per_day: self.budget_usd_per_day,
        }
    }

    pub fn apply_view(&mut self, u: eir_proto::AdvisorSettingsUpdate) {
        self.enabled = u.enabled;
        self.escalation_model = u.escalation_model.trim().to_string();
        self.escalation_effort = normalize_effort(&u.escalation_effort);
        self.low_confidence_threshold = u.low_confidence_threshold.clamp(0.0, 0.95);
        self.budget_usd_per_day = u.budget_usd_per_day.max(0.0);
    }
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
pub enum ApiProvider {
    /// Anthropic native API (`/v1/messages`). Legacy `claude_cli` /
    /// `openai_compatible` configs alias here so an old config.toml still loads
    /// (they then need a key set in Settings before analysis resumes).
    #[default]
    #[serde(
        rename = "anthropic",
        alias = "claude_cli",
        alias = "openai_compatible",
        alias = "open_ai_compatible"
    )]
    Anthropic,
    /// OpenRouter (openrouter.ai) — OpenAI-compatible; supports free models.
    #[serde(rename = "openrouter", alias = "open_router")]
    OpenRouter,
    /// Kilo Code gateway (api.kilo.ai) — OpenAI-compatible, 500+ models.
    #[serde(rename = "kilocode", alias = "kilo")]
    KiloCode,
}

impl ApiProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiProvider::Anthropic => "anthropic",
            ApiProvider::OpenRouter => "openrouter",
            ApiProvider::KiloCode => "kilocode",
        }
    }

    fn parse(s: &str) -> ApiProvider {
        match s {
            "openrouter" | "open_router" => ApiProvider::OpenRouter,
            "kilocode" | "kilo" => ApiProvider::KiloCode,
            _ => ApiProvider::Anthropic,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub provider: ApiProvider,
    /// Anthropic native: API key from console.anthropic.com
    pub anthropic_api_key: Option<String>,
    /// OpenRouter API key (provider = "openrouter").
    pub openrouter_api_key: Option<String>,
    /// Kilo Code gateway API key from app.kilo.ai (provider = "kilocode").
    #[serde(default)]
    pub kilocode_api_key: Option<String>,
    /// Model name. Empty = a provider default (OpenRouter: openrouter/free).
    #[serde(default)]
    pub model: String,
    /// Model for the on-demand app-update check (empty = a cheap provider default).
    #[serde(default)]
    pub update_check_model: String,
    /// Reasoning effort: low|medium|high|xhigh|max, empty = provider default.
    /// Maps to `output_config.effort` (Anthropic) / `reasoning.effort`
    /// (OpenRouter, Kilo Code); models without a reasoning dial may reject it.
    #[serde(default)]
    pub effort: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MonitoringConfig {
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub decision_interval_secs: u64,
    /// Minimum AI confidence (0.0–1.0) for a whitelisted fix to auto-execute.
    /// Overrides the fallback in policy.toml; editable from the app's Settings.
    #[serde(default = "default_confidence")]
    pub confidence_threshold: f32,
}

fn default_confidence() -> f32 {
    0.80
}

/// Accept only the known reasoning-effort levels; anything else (incl. blank)
/// becomes empty, i.e. the provider default. Keeps an invalid value from being
/// sent straight to a provider API.
fn normalize_effort(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        e @ ("low" | "medium" | "high" | "xhigh" | "max") => e.to_string(),
        _ => String::new(),
    }
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
            effort: self.api.effort.clone(),
            decision_interval_secs: self.monitoring.decision_interval_secs,
            event_log_poll_interval_secs: self.monitoring.event_log_poll_interval_secs,
            wmi_poll_interval_secs: self.monitoring.wmi_poll_interval_secs,
            event_log_channels: self.monitoring.event_log_channels.clone(),
            log_directories: self.monitoring.log_directories.clone(),
            confidence_threshold: self.monitoring.confidence_threshold,
            openrouter_key_set: set(&self.api.openrouter_api_key),
            anthropic_key_set: set(&self.api.anthropic_api_key),
            kilocode_key_set: set(&self.api.kilocode_api_key),
            // Deprecated wire fields (see eir_proto::UiSettings).
            base_url: String::new(),
            api_key_set: false,
        }
    }

    /// Apply an update from the UI. Empty/None secret fields keep the stored value.
    pub fn apply_update(&mut self, u: SettingsUpdate) {
        self.api.provider = ApiProvider::parse(&u.provider);
        self.api.model = u.model;
        self.api.update_check_model = u.update_check_model;
        self.api.effort = normalize_effort(&u.effort);
        let keep = |cur: &mut Option<String>, new: Option<String>| {
            if let Some(v) = new {
                if !v.trim().is_empty() {
                    *cur = Some(v);
                }
            }
        };
        keep(&mut self.api.openrouter_api_key, u.openrouter_api_key);
        keep(&mut self.api.anthropic_api_key, u.anthropic_api_key);
        keep(&mut self.api.kilocode_api_key, u.kilocode_api_key);
        self.monitoring.decision_interval_secs = u.decision_interval_secs.max(10);
        self.monitoring.event_log_poll_interval_secs = u.event_log_poll_interval_secs.max(5);
        self.monitoring.wmi_poll_interval_secs = u.wmi_poll_interval_secs.max(30);
        self.monitoring.event_log_channels = u.event_log_channels;
        self.monitoring.log_directories = u.log_directories;
        // Clamp to a sane range: never 0 (would auto-run everything) nor ≥1.0
        // (would never run anything).
        self.monitoring.confidence_threshold = u.confidence_threshold.clamp(0.50, 0.95);
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
provider = "anthropic"
model = ""
[monitoring]
event_log_channels = ["System"]
log_directories = []
event_log_poll_interval_secs = 30
wmi_poll_interval_secs = 300
decision_interval_secs = 600
[persistence]
audit_db = "./eir.db"
"#;

    #[test]
    fn apply_update_then_toml_round_trips() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "openrouter".into(),
            model: "nvidia/nemotron-3-super-120b-a12b:free".into(),
            update_check_model: "claude-haiku-4-5".into(),
            effort: "High".into(),
            openrouter_api_key: Some("sk-or-test".into()),
            anthropic_api_key: None,
            kilocode_api_key: None,
            decision_interval_secs: 900,
            event_log_poll_interval_secs: 45,
            wmi_poll_interval_secs: 300,
            event_log_channels: vec!["System".into(), "Application".into()],
            log_directories: vec!["C:\\Logs".into()],
            confidence_threshold: 0.9,
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
        assert_eq!(reparsed.monitoring.confidence_threshold, 0.9);
        assert_eq!(reparsed.api.update_check_model, "claude-haiku-4-5");
        // Effort is normalised (case-folded) and round-trips.
        assert_eq!(reparsed.api.effort, "high");
        assert_eq!(reparsed.monitoring.event_log_channels.len(), 2);
        // Blank api key keeps the prior value (None here).
        assert!(reparsed.api.anthropic_api_key.is_none());
    }

    #[test]
    fn config_without_updater_section_loads_defaults_and_round_trips() {
        // SAMPLE has no [updater] section — it must still load (serde default),
        // and once written back it must reparse identically.
        use crate::updater::config::SignaturePolicy;
        let cfg: Config = toml::from_str(SAMPLE).expect("load without [updater]");
        assert!(!cfg.updater.enabled, "updater is off by default");
        assert_eq!(
            cfg.updater.native_signature_policy,
            SignaturePolicy::RequireValid
        );
        assert!(cfg.updater.methods.contains(&"winget".to_string()));

        let serialized = toml::to_string_pretty(&cfg).expect("serialize with [updater]");
        let reparsed: Config = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reparsed.updater.enabled, cfg.updater.enabled);
        assert_eq!(reparsed.updater.methods, cfg.updater.methods);
        assert_eq!(reparsed.updater.notes, cfg.updater.notes);
    }

    #[test]
    fn legacy_open_router_provider_alias_still_parses() {
        // Older configs serialized OpenRouter as "open_router"; must still load.
        let toml = SAMPLE.replace("\"anthropic\"", "\"open_router\"");
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.api.provider.as_str(), "openrouter");
    }

    #[test]
    fn removed_providers_alias_to_anthropic() {
        // A config written by an older build (claude_cli / openai_compatible,
        // possibly with now-removed extra fields like base_url/user_profile)
        // must still load — those providers alias to Anthropic and unknown
        // keys are ignored by the toml loader.
        for legacy in ["claude_cli", "openai_compatible"] {
            let src = SAMPLE.replace(
                "provider = \"anthropic\"",
                &format!(
                    "provider = \"{legacy}\"\nbase_url = \"http://localhost:8080/v1\"\nuser_profile = 'C:\\Users\\X'"
                ),
            );
            let cfg: Config = toml::from_str(&src).unwrap();
            assert_eq!(cfg.api.provider.as_str(), "anthropic");
        }
    }

    #[test]
    fn kilocode_provider_round_trips() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "kilocode".into(),
            model: "anthropic/claude-sonnet-4.6".into(),
            kilocode_api_key: Some("kilo-test-key".into()),
            ..Default::default()
        });
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::KiloCode);
        assert_eq!(
            reparsed.api.kilocode_api_key.as_deref(),
            Some("kilo-test-key")
        );
        assert!(reparsed.to_ui_settings().kilocode_key_set);
    }
}
