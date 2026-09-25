use crate::updater::config::UpdaterConfig;
use anyhow::{anyhow, Context, Result};
use eir_proto::{SettingsUpdate, UiSettings};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

static RUNTIME_ROOT: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Deserialize, Serialize, Clone)]
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
    /// Linux only: the Unix-socket control surface `eirctl` connects to. Ignored on
    /// Windows (which keeps its named pipe). `#[serde(default)]` so a `config.toml`
    /// without a `[service]` section still loads.
    #[serde(default)]
    pub service: ServiceConfig,
    /// Linux only: an optional external command Eir invokes to alert the owner
    /// (Discord/Telegram/etc). Disabled (empty) by default. Ignored on Windows.
    /// `#[serde(default)]` so a `config.toml` without a `[notify]` section still loads.
    #[serde(default)]
    pub notify: NotifyConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct ServiceConfig {
    /// Unix-domain socket path eirctl connects to.
    pub socket_path: String,
    /// Additional uids (beyond uid 0/root, always implicitly allowed) permitted to
    /// connect to the control socket.
    pub socket_allow_uids: Vec<u32>,
    /// Additional gids permitted to connect to the control socket.
    pub socket_allow_gids: Vec<u32>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            socket_path: "/run/eir/eir.sock".to_string(),
            socket_allow_uids: Vec::new(),
            socket_allow_gids: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(default)]
pub struct NotifyConfig {
    /// `argv` of an external alert command; empty (the default) disables the hook.
    /// Eir appends the alert text as the final argument at call time — see
    /// `notify::send` — never interpolated into the command itself.
    pub command: Vec<String>,
}

/// When the AI flags a hard/ambiguous situation (or its confidence is low), Eir can
/// re-analyse once at a higher reasoning effort and/or a stronger model. Off by
/// default; the escalation tier is fixed config (never AI-chosen). A hard count
/// cap (MAX_ESCALATIONS_PER_DAY) remains the only backstop.
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
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            escalation_model: String::new(),
            escalation_effort: String::new(),
            low_confidence_threshold: 0.6,
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
        }
    }

    pub fn apply_view(&mut self, u: eir_proto::AdvisorSettingsUpdate) {
        self.enabled = u.enabled;
        self.escalation_model = u.escalation_model.trim().to_string();
        self.escalation_effort = normalize_effort(&u.escalation_effort);
        self.low_confidence_threshold = finite_or(u.low_confidence_threshold, 0.0, 0.95, 0.6);
    }
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
pub enum ApiProvider {
    /// OpenCode via the local `opencode` CLI — no API key; models can include
    /// cloud providers and local `ollama/...`. Legacy openrouter / kilo / ollama
    /// provider names alias here so an old config.toml still loads.
    #[default]
    #[serde(
        rename = "opencode_cli",
        alias = "openrouter",
        alias = "open_router",
        alias = "kilo_cli",
        alias = "kilocode",
        alias = "kilo",
        alias = "ollama"
    )]
    OpenCode,
    /// Claude via the local `claude` CLI — no API key, uses the machine's
    /// logged-in Claude subscription session. Legacy `anthropic` /
    /// `openai_compatible` names alias here.
    #[serde(
        rename = "claude_cli",
        alias = "anthropic",
        alias = "openai_compatible",
        alias = "open_ai_compatible"
    )]
    Claude,
    /// Codex via the local `codex` CLI — no API key, uses the machine's
    /// logged-in ChatGPT subscription session.
    #[serde(rename = "codex_cli")]
    Codex,
    /// Cursor Agent via the local `agent` CLI — no API key; uses the machine's
    /// logged-in Cursor subscription session.
    #[serde(rename = "cursor_cli")]
    Cursor,
}

impl ApiProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiProvider::OpenCode => "opencode_cli",
            ApiProvider::Claude => "claude_cli",
            ApiProvider::Codex => "codex_cli",
            ApiProvider::Cursor => "cursor_cli",
        }
    }

    fn parse(s: &str) -> ApiProvider {
        match s {
            "claude_cli" | "anthropic" | "openai_compatible" | "open_ai_compatible" => {
                ApiProvider::Claude
            }
            "codex_cli" => ApiProvider::Codex,
            "cursor_cli" => ApiProvider::Cursor,
            // Default + legacy HTTP/CLI providers that now map to OpenCode.
            _ => ApiProvider::OpenCode,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ApiConfig {
    #[serde(default)]
    pub provider: ApiProvider,
    /// Model name. Empty = provider / CLI default.
    #[serde(default)]
    pub model: String,
    /// Model for the on-demand app-update check (empty = a cheap provider default).
    #[serde(default)]
    pub update_check_model: String,
    /// Reasoning effort: low|medium|high|xhigh|max, empty = provider default.
    /// Maps to `--effort` (Claude CLI), `model_reasoning_effort` (Codex CLI),
    /// or `--variant` (OpenCode CLI); Cursor has no effort dial.
    #[serde(default)]
    pub effort: String,
    /// claude_cli: path to the claude binary. Blank = auto-detect
    /// (`<profile>\.local\bin\claude.exe`, then PATH).
    #[serde(default)]
    pub claude_cli_path: Option<String>,
    /// claude_cli: optional profile hint for interactive/dev runs. The LocalSystem
    /// service always uses the sole active desktop user's profile and token.
    #[serde(default)]
    pub user_profile: Option<String>,
    /// codex_cli: path to the codex binary. Blank = auto-detect under the
    /// resolved user profile, then PATH.
    #[serde(default)]
    pub codex_cli_path: Option<String>,
    /// opencode_cli: path to the `opencode` binary. Blank = auto-detect.
    #[serde(default)]
    pub opencode_cli_path: Option<String>,
    /// opencode_cli: optional profile hint for interactive/dev runs.
    #[serde(default)]
    pub opencode_cli_user_profile: Option<String>,
    /// cursor_cli: path to the `agent` binary. Blank = auto-detect
    /// (`<profile>\.local\bin\agent.cmd`, then PATH).
    #[serde(default)]
    pub cursor_cli_path: Option<String>,
    /// cursor_cli: optional profile hint for interactive/dev runs.
    #[serde(default)]
    pub cursor_cli_user_profile: Option<String>,
    /// Linux only: the local system user the AI CLI runs as (the privilege-drop
    /// target) when eir-svc itself runs as root under systemd. Required non-empty in
    /// that case — see `ai::cli_user_launch_unix`'s fail-closed startup guard, which
    /// refuses to launch the CLI as root rather than silently skipping the drop.
    /// Ignored on Windows.
    #[serde(default)]
    pub linux_ai_user: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MonitoringConfig {
    #[serde(default)]
    pub event_log_channels: Vec<String>,
    #[serde(default)]
    pub log_directories: Vec<String>,
    #[serde(default = "default_el_poll")]
    pub event_log_poll_interval_secs: u64,
    #[serde(default = "default_wmi_poll")]
    pub wmi_poll_interval_secs: u64,
    #[serde(default = "default_decision_interval")]
    pub decision_interval_secs: u64,
    /// Minimum AI confidence (0.0–1.0) for a whitelisted fix to auto-execute.
    /// Overrides the fallback in policy.toml; editable from the app's Settings.
    #[serde(default = "default_confidence")]
    pub confidence_threshold: f32,
    /// Auto-enable Game Mode when the tray detects a fullscreen game/app. On by default —
    /// it only makes Eir quieter (defers updater/digest), never destructive.
    #[serde(default = "default_true")]
    pub game_mode_auto: bool,
    /// While Game Mode is active, switch to the High Performance power plan and restore it
    /// on exit. Off by default (marginal on desktops; changes display/sleep timeouts).
    #[serde(default)]
    pub game_mode_power_boost: bool,
    /// Run decision cycles, the autonomous updater and the digest only while the tray app
    /// is connected. On by default: with no tray there is nobody to see findings or approve
    /// fixes, so the service keeps collecting signals and resumes the moment the tray opens.
    /// Set false for a headless machine that should self-heal unattended. Defaults to
    /// `false` on Linux (there is no tray to connect at all) and `true` on Windows —
    /// a dedicated default fn, not the shared `default_true` above (which stays used
    /// by `game_mode_auto`/`watch_screen_errors`, whose default is `true` on both OSes).
    #[serde(default = "default_require_tray")]
    pub require_tray: bool,
    /// Have the tray report error message boxes and hung ("Not Responding") windows so
    /// Eir reacts to the errors the user actually sees. On by default; the text goes to
    /// the configured AI provider like log excerpts do.
    #[serde(default = "default_true")]
    pub watch_screen_errors: bool,
}

fn default_confidence() -> f32 {
    0.80
}

fn default_true() -> bool {
    true
}

#[cfg(windows)]
fn default_require_tray() -> bool {
    true
}
#[cfg(unix)]
fn default_require_tray() -> bool {
    false
}

fn default_el_poll() -> u64 {
    30
}
fn default_wmi_poll() -> u64 {
    300
}
fn default_decision_interval() -> u64 {
    600
}

/// Accept only the known reasoning-effort levels; anything else (incl. blank)
/// becomes empty, i.e. the provider default. Keeps an invalid value from being
/// sent straight to a provider API.
/// Treats the shipped example config placeholder `YourName` as "not set", so
/// the UI's "configured" dot doesn't lie when a user copies the template and
/// forgets to fill in their own Windows profile / binary path.
fn is_real(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty() && !v.contains("YourName")
}

fn normalize_effort(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        e @ ("low" | "medium" | "high" | "xhigh" | "max") => e.to_string(),
        _ => String::new(),
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PersistenceConfig {
    pub audit_db: String,
}

fn finite_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

const MAX_LOG_DIRECTORIES: usize = 16;
const MAX_LOG_DIRECTORY_CHARS: usize = 1024;
const MAX_EVENT_LOG_CHANNELS: usize = 64;
const MAX_EVENT_LOG_CHANNEL_CHARS: usize = 256;
const MAX_MONITORING_INTERVAL_SECS: u64 = 7 * 24 * 3600;

fn normalize_event_log_channels(channels: Vec<String>) -> Result<Vec<String>> {
    if channels.len() > MAX_EVENT_LOG_CHANNELS {
        anyhow::bail!("at most {MAX_EVENT_LOG_CHANNELS} event-log channels are allowed");
    }
    let mut seen = HashSet::new();
    let mut safe = Vec::new();
    for raw in channels {
        let channel = raw.trim();
        if channel.is_empty() {
            continue;
        }
        if channel.chars().count() > MAX_EVENT_LOG_CHANNEL_CHARS {
            anyhow::bail!("event-log channel name is too long");
        }
        if channel.chars().any(char::is_control) {
            anyhow::bail!("event-log channel name contains control characters");
        }
        if seen.insert(channel.to_ascii_lowercase()) {
            safe.push(channel.to_string());
        }
    }
    Ok(safe)
}

fn sanitize_loaded_event_log_channels(channels: Vec<String>) -> Vec<String> {
    if channels.len() > MAX_EVENT_LOG_CHANNELS {
        tracing::warn!(
            "Ignoring configured event-log channels past the {MAX_EVENT_LOG_CHANNELS}-channel limit"
        );
    }
    let mut safe = Vec::new();
    let mut seen = HashSet::new();
    for raw in channels.into_iter().take(MAX_EVENT_LOG_CHANNELS) {
        match normalize_event_log_channels(vec![raw]) {
            Ok(channels) => {
                for channel in channels {
                    if seen.insert(channel.to_ascii_lowercase()) {
                        safe.push(channel);
                    }
                }
            }
            Err(error) => tracing::warn!("Ignoring unsafe event-log channel: {error}"),
        }
    }
    safe
}

fn normalize_log_directories(directories: Vec<String>) -> Result<Vec<String>> {
    if directories.len() > MAX_LOG_DIRECTORIES {
        anyhow::bail!("at most {MAX_LOG_DIRECTORIES} log directories are allowed");
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in directories {
        let path = raw.trim();
        if path.is_empty() {
            continue;
        }
        if path.chars().count() > MAX_LOG_DIRECTORY_CHARS {
            anyhow::bail!("log directory is too long");
        }
        let parsed = std::path::Path::new(path);
        let mut components = parsed.components();
        let local_drive = matches!(components.next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_)))
            && matches!(components.next(), Some(Component::RootDir));
        if !local_drive {
            anyhow::bail!("log directory must be an absolute local drive path: {path}");
        }
        let key = path.replace('/', "\\").to_ascii_lowercase();
        if seen.insert(key) {
            out.push(path.to_string());
        }
    }
    Ok(out)
}

fn sanitize_loaded_log_directories(directories: Vec<String>) -> Vec<String> {
    let mut safe = Vec::new();
    let mut seen = HashSet::new();
    for raw in directories {
        if safe.len() >= MAX_LOG_DIRECTORIES {
            tracing::warn!(
                "Ignoring configured log directories past the {MAX_LOG_DIRECTORIES}-root limit"
            );
            break;
        }
        match normalize_log_directories(vec![raw]) {
            Ok(paths) => {
                for path in paths {
                    let key = path.replace('/', "\\").to_ascii_lowercase();
                    if seen.insert(key) {
                        safe.push(path);
                    }
                }
            }
            Err(e) => tracing::warn!("Ignoring unsafe legacy log directory: {e}"),
        }
    }
    safe
}

impl Config {
    /// Current settings for the UI (no secrets — only whether they are set).
    pub fn to_ui_settings(&self) -> UiSettings {
        UiSettings {
            provider: self.api.provider.as_str().to_string(),
            model: self.api.model.clone(),
            update_check_model: self.api.update_check_model.clone(),
            // Normalise on the way out too: a hand-edited `effort = "HIGH"` would
            // otherwise be sent verbatim and render blank in the UI's <select>.
            effort: normalize_effort(&self.api.effort),
            decision_interval_secs: self.monitoring.decision_interval_secs,
            event_log_poll_interval_secs: self.monitoring.event_log_poll_interval_secs,
            wmi_poll_interval_secs: self.monitoring.wmi_poll_interval_secs,
            event_log_channels: self.monitoring.event_log_channels.clone(),
            log_directories: self.monitoring.log_directories.clone(),
            confidence_threshold: self.monitoring.confidence_threshold,
            // Deprecated wire fields (always empty/false — see eir_proto::UiSettings).
            openrouter_key_set: false,
            anthropic_key_set: false,
            kilo_cli_user_profile_set: false,
            kilo_cli_path_set: false,
            ollama_base_url: String::new(),
            ollama_key_set: false,
            opencode_cli_path_set: self.api.opencode_cli_path.as_deref().is_some_and(is_real),
            opencode_cli_user_profile_set: self
                .api
                .opencode_cli_user_profile
                .as_deref()
                .is_some_and(is_real),
            cursor_cli_path_set: self.api.cursor_cli_path.as_deref().is_some_and(is_real),
            cursor_cli_user_profile_set: self
                .api
                .cursor_cli_user_profile
                .as_deref()
                .is_some_and(is_real),
            // Deprecated wire fields (see eir_proto::UiSettings).
            base_url: String::new(),
            api_key_set: false,
            game_mode_auto: self.monitoring.game_mode_auto,
            game_mode_power_boost: self.monitoring.game_mode_power_boost,
            watch_screen_errors: self.monitoring.watch_screen_errors,
        }
    }

    /// Whether applying this update would change fields that can only be picked up
    /// by restarting the service process (collector spawn parameters).
    pub fn settings_update_needs_restart(&self, u: &SettingsUpdate) -> bool {
        self.monitoring.event_log_channels != u.event_log_channels
            || self.monitoring.log_directories != u.log_directories
            || self.monitoring.event_log_poll_interval_secs != u.event_log_poll_interval_secs
            || self.monitoring.wmi_poll_interval_secs != u.wmi_poll_interval_secs
    }

    /// Apply an update from the UI. Empty/None secret fields keep the stored value.
    pub fn apply_update(&mut self, u: SettingsUpdate) -> Result<()> {
        let event_log_channels = normalize_event_log_channels(u.event_log_channels)?;
        let log_directories = normalize_log_directories(u.log_directories)?;
        let provider = ApiProvider::parse(&u.provider);
        if provider != self.api.provider {
            // An escalation model belongs to the old provider and may be invalid
            // for the new one. Blank safely means "keep the base model".
            self.advisor.escalation_model.clear();
        }
        self.api.provider = provider;
        self.api.model = u.model;
        self.api.update_check_model = u.update_check_model;
        self.api.effort = normalize_effort(&u.effort);
        // Blank/whitespace means "unchanged" for path/profile overrides (the UI
        // sends blank on every unrelated save).
        let keep = |cur: &mut Option<String>, new: Option<String>| {
            if let Some(v) = new {
                let v = v.trim();
                if !v.is_empty() {
                    *cur = Some(v.to_string());
                }
            }
        };
        keep(&mut self.api.opencode_cli_path, u.opencode_cli_path);
        keep(
            &mut self.api.opencode_cli_user_profile,
            u.opencode_cli_user_profile,
        );
        keep(&mut self.api.cursor_cli_path, u.cursor_cli_path);
        keep(
            &mut self.api.cursor_cli_user_profile,
            u.cursor_cli_user_profile,
        );
        self.monitoring.decision_interval_secs = u
            .decision_interval_secs
            .clamp(10, MAX_MONITORING_INTERVAL_SECS);
        self.monitoring.event_log_poll_interval_secs = u
            .event_log_poll_interval_secs
            .clamp(5, MAX_MONITORING_INTERVAL_SECS);
        self.monitoring.wmi_poll_interval_secs = u
            .wmi_poll_interval_secs
            .clamp(30, MAX_MONITORING_INTERVAL_SECS);
        self.monitoring.event_log_channels = event_log_channels;
        self.monitoring.log_directories = log_directories;
        // Clamp to a sane range: never 0 (would auto-run everything) nor ≥1.0
        // (would never run anything).
        self.monitoring.confidence_threshold =
            finite_or(u.confidence_threshold, 0.50, 0.95, default_confidence());
        self.monitoring.game_mode_auto = u.game_mode_auto;
        self.monitoring.game_mode_power_boost = u.game_mode_power_boost;
        if let Some(on) = u.watch_screen_errors {
            self.monitoring.watch_screen_errors = on;
        }
        Ok(())
    }
}

/// Write the config back to disk (resolved relative to the exe directory).
///
/// Atomic: the new TOML is written to a sibling temp file and `rename`d over the live
/// config (a same-directory rename replaces atomically on NTFS), so a crash or SCM
/// force-kill mid-write can never leave a truncated, unparseable `config.toml`. The
/// previous *parseable* config is preserved as `config.toml.bak` first, which [`load`]
/// falls back to if the live file is ever found corrupt.
pub fn save(config: &Config, path: &str) -> Result<()> {
    let resolved = resolve(path);
    let toml = toml::to_string_pretty(config).context("Failed to serialize config")?;

    // Keep the current config as a recovery source — but only if it currently parses,
    // so a corrupt live file can never overwrite a good backup.
    if let Ok(existing) = fs::read_to_string(&resolved) {
        if toml::from_str::<Config>(&existing).is_ok() {
            let bak = resolved.with_extension("toml.bak");
            let _ = fs::write(&bak, &existing);
        }
    }

    let tmp = resolved.with_extension("toml.tmp");
    fs::write(&tmp, &toml)
        .with_context(|| format!("Failed to write temp config file: {}", tmp.display()))?;
    fs::rename(&tmp, &resolved)
        .with_context(|| format!("Failed to replace config file: {}", resolved.display()))?;
    Ok(())
}

pub async fn save_async(config: &Config, path: &str) -> Result<()> {
    let config = config.clone();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || save(&config, &path)).await?
}

pub fn set_runtime_root(root: PathBuf) -> Result<()> {
    RUNTIME_ROOT
        .set(root)
        .map_err(|_| anyhow!("runtime root is already configured"))
}

fn resolve_at(rel: &str, runtime_root: Option<&Path>, executable: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(rel);
    if p.is_absolute() {
        return p;
    }
    runtime_root
        .map(|root| root.join(rel))
        .or_else(|| executable.and_then(Path::parent).map(|dir| dir.join(rel)))
        .unwrap_or(p)
}

/// Resolve relative state paths beneath the configured runtime root, or beside
/// the executable for installed/development runs. Absolute paths are unchanged.
pub fn resolve(rel: &str) -> PathBuf {
    let executable = std::env::current_exe().ok();
    resolve_at(
        rel,
        RUNTIME_ROOT.get().map(PathBuf::as_path),
        executable.as_deref(),
    )
}

fn sanitize_loaded(mut cfg: Config) -> Config {
    cfg.monitoring.event_log_channels =
        sanitize_loaded_event_log_channels(cfg.monitoring.event_log_channels);
    cfg.monitoring.log_directories =
        sanitize_loaded_log_directories(cfg.monitoring.log_directories);
    cfg.monitoring.decision_interval_secs = cfg
        .monitoring
        .decision_interval_secs
        .clamp(10, MAX_MONITORING_INTERVAL_SECS);
    cfg.monitoring.event_log_poll_interval_secs = cfg
        .monitoring
        .event_log_poll_interval_secs
        .clamp(5, MAX_MONITORING_INTERVAL_SECS);
    cfg.monitoring.wmi_poll_interval_secs = cfg
        .monitoring
        .wmi_poll_interval_secs
        .clamp(30, MAX_MONITORING_INTERVAL_SECS);
    cfg.monitoring.confidence_threshold = finite_or(
        cfg.monitoring.confidence_threshold,
        0.50,
        0.95,
        default_confidence(),
    );
    cfg.advisor.low_confidence_threshold = finite_or(
        cfg.advisor.low_confidence_threshold,
        0.0,
        0.95,
        AdvisorConfig::default().low_confidence_threshold,
    );
    cfg.updater.sanitize();
    // Bare model ids with no provider/ prefix were only valid for the removed
    // standalone Ollama provider; OpenCode expects `ollama/<name>`.
    if cfg.api.provider == ApiProvider::OpenCode {
        let model = cfg.api.model.trim();
        if !model.is_empty() && !model.contains('/') {
            cfg.api.model = format!("ollama/{model}");
        }
        let upd = cfg.api.update_check_model.trim();
        if !upd.is_empty() && !upd.contains('/') {
            cfg.api.update_check_model = format!("ollama/{upd}");
        }
    }
    cfg
}

pub fn load(path: &str) -> Result<Config> {
    let resolved = resolve(path);
    let contents = fs::read_to_string(&resolved)
        .with_context(|| format!("Failed to read config file: {}", resolved.display()))?;
    match toml::from_str::<Config>(&contents) {
        Ok(cfg) => Ok(sanitize_loaded(cfg)),
        Err(primary) => {
            // The live config didn't parse (e.g. truncated by a crash mid-write).
            // Recover from the last-known-good backup rather than going fatal — a
            // self-healing service should not be bricked by its own config write.
            let bak = resolved.with_extension("toml.bak");
            let recovered = fs::read_to_string(&bak)
                .ok()
                .and_then(|c| toml::from_str::<Config>(&c).ok());
            match recovered {
                Some(cfg) => {
                    tracing::warn!(
                        "config.toml failed to parse ({primary}); recovered from {}",
                        bak.display()
                    );
                    Ok(sanitize_loaded(cfg))
                }
                None => Err(primary).context("Failed to parse config TOML"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[api]
provider = "opencode_cli"
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
    fn shipped_config_toml_example_parses() {
        let example = include_str!("../../config.toml.example");
        toml::from_str::<Config>(example).expect("config.toml.example must deserialize");
    }

    #[cfg(windows)]
    #[test]
    fn portable_root_overrides_executable_directory_for_relative_state() {
        let portable_root = std::path::Path::new(r"C:\Users\Alice\AppData\Local\EirPortable");
        let executable = std::path::Path::new(r"C:\Temp\IXP001.TMP\eir-svc.exe");

        assert_eq!(
            resolve_at("config.toml", Some(portable_root), Some(executable)),
            portable_root.join("config.toml")
        );
        assert_eq!(
            resolve_at(r"D:\state\eir.db", Some(portable_root), Some(executable)),
            std::path::PathBuf::from(r"D:\state\eir.db")
        );
    }

    #[cfg(windows)]
    #[test]
    fn apply_update_then_toml_round_trips() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "opencode_cli".into(),
            model: "ollama/llama3.2".into(),
            update_check_model: "ollama/llama3.2".into(),
            effort: "High".into(),
            opencode_cli_path: Some(r"C:\tools\opencode.exe".into()),
            opencode_cli_user_profile: None,
            cursor_cli_path: None,
            cursor_cli_user_profile: None,
            decision_interval_secs: 900,
            event_log_poll_interval_secs: 45,
            wmi_poll_interval_secs: 300,
            event_log_channels: vec!["System".into(), "Application".into()],
            log_directories: vec!["C:\\Logs".into()],
            confidence_threshold: 0.9,
            game_mode_auto: true,
            game_mode_power_boost: false,
            ..Default::default()
        })
        .unwrap();
        // Must serialize to TOML the loader can read back (else a settings save bricks the service).
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider.as_str(), "opencode_cli");
        assert_eq!(reparsed.api.model, "ollama/llama3.2");
        assert_eq!(
            reparsed.api.opencode_cli_path.as_deref(),
            Some(r"C:\tools\opencode.exe")
        );
        assert_eq!(reparsed.monitoring.decision_interval_secs, 900);
        assert_eq!(reparsed.monitoring.confidence_threshold, 0.9);
        assert_eq!(reparsed.api.update_check_model, "ollama/llama3.2");
        // Effort is normalised (case-folded) and round-trips.
        assert_eq!(reparsed.api.effort, "high");
        assert_eq!(reparsed.monitoring.event_log_channels.len(), 2);
    }

    #[test]
    fn settings_restart_needed_only_for_collector_fields() {
        let cfg: Config = toml::from_str(SAMPLE).unwrap();
        // Provider/model/key/effort/threshold/decision-interval do NOT need restart.
        let no_restart = SettingsUpdate {
            provider: "opencode_cli".into(),
            model: "x".into(),
            update_check_model: "y".into(),
            effort: "high".into(),
            opencode_cli_path: None,
            opencode_cli_user_profile: None,
            cursor_cli_path: None,
            cursor_cli_user_profile: None,
            decision_interval_secs: 900,
            event_log_poll_interval_secs: 30,
            wmi_poll_interval_secs: 300,
            event_log_channels: vec!["System".into()],
            log_directories: vec![],
            confidence_threshold: 0.7,
            game_mode_auto: true,
            game_mode_power_boost: false,
            ..Default::default()
        };
        assert!(!cfg.settings_update_needs_restart(&no_restart));
        // Each collector field individually triggers restart.
        let mut changed = no_restart.clone();
        changed.event_log_channels = vec!["System".into(), "Application".into()];
        assert!(cfg.settings_update_needs_restart(&changed));
        changed = no_restart.clone();
        changed.log_directories = vec!["C:\\Logs".into()];
        assert!(cfg.settings_update_needs_restart(&changed));
        changed = no_restart.clone();
        changed.event_log_poll_interval_secs = 60;
        assert!(cfg.settings_update_needs_restart(&changed));
        changed = no_restart.clone();
        changed.wmi_poll_interval_secs = 600;
        assert!(cfg.settings_update_needs_restart(&changed));
    }

    #[cfg(windows)]
    #[test]
    fn configured_log_roots_are_local_bounded_and_deduplicated() {
        let roots = normalize_log_directories(vec![
            r"C:\Logs".into(),
            r" c:\logs ".into(),
            r"D:\Games\App\logs".into(),
        ])
        .expect("local roots");
        assert_eq!(roots, vec![r"C:\Logs", r"D:\Games\App\logs"]);

        for bad in [
            vec![r"\\server\share".into()],
            vec!["//server/share".into()],
            vec![r"relative\logs".into()],
            vec![r"\\?\C:\Logs".into()],
        ] {
            assert!(normalize_log_directories(bad).is_err());
        }

        let too_many: Vec<String> = (0..=MAX_LOG_DIRECTORIES)
            .map(|i| format!(r"C:\Logs\{i}"))
            .collect();
        assert!(normalize_log_directories(too_many).is_err());
    }

    #[test]
    fn event_log_channels_reject_unbounded_or_control_character_input() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        let before = toml::to_string(&cfg).unwrap();
        let too_many = (0..=MAX_EVENT_LOG_CHANNELS)
            .map(|index| format!("Channel-{index}"))
            .collect();
        assert!(cfg
            .apply_update(SettingsUpdate {
                event_log_channels: too_many,
                ..Default::default()
            })
            .is_err());
        assert_eq!(toml::to_string(&cfg).unwrap(), before);

        assert!(cfg
            .apply_update(SettingsUpdate {
                event_log_channels: vec!["System\nInjected".into()],
                ..Default::default()
            })
            .is_err());
        assert_eq!(toml::to_string(&cfg).unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn unsafe_legacy_log_roots_are_skipped_without_bricking_config() {
        let roots = sanitize_loaded_log_directories(vec![
            r"relative\logs".into(),
            r"\\server\share".into(),
            r"C:\Logs".into(),
            r"c:\logs".into(),
        ]);
        assert_eq!(roots, [r"C:\Logs"]);
    }

    #[test]
    fn rejected_settings_update_does_not_partially_mutate_config() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        let before = toml::to_string(&cfg).unwrap();
        let update = SettingsUpdate {
            provider: "codex_cli".into(),
            model: "changed".into(),
            log_directories: vec![r"\\server\share".into()],
            ..Default::default()
        };
        assert!(cfg.apply_update(update).is_err());
        assert_eq!(toml::to_string(&cfg).unwrap(), before);
    }

    #[test]
    fn extreme_settings_values_are_bounded_before_runtime_use() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            decision_interval_secs: u64::MAX,
            event_log_poll_interval_secs: u64::MAX,
            wmi_poll_interval_secs: u64::MAX,
            confidence_threshold: f32::NAN,
            ..Default::default()
        })
        .expect("apply");
        assert_eq!(
            cfg.monitoring.decision_interval_secs,
            MAX_MONITORING_INTERVAL_SECS
        );
        assert_eq!(
            cfg.monitoring.event_log_poll_interval_secs,
            MAX_MONITORING_INTERVAL_SECS
        );
        assert_eq!(
            cfg.monitoring.wmi_poll_interval_secs,
            MAX_MONITORING_INTERVAL_SECS
        );
        assert!(cfg.monitoring.confidence_threshold.is_finite());
    }

    #[test]
    fn corrupt_config_recovers_from_backup() {
        // Simulate a crash that truncated config.toml mid-write: save a good config
        // (which writes the .bak), clobber the live file with garbage, and confirm
        // load() recovers from the backup instead of erroring.
        let dir = std::env::temp_dir().join(format!("eir-cfg-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let path_str = path.to_string_lossy().to_string();

        let mut good: Config = toml::from_str(SAMPLE).unwrap();
        good.monitoring.decision_interval_secs = 555;
        // First save has no existing file to back up.
        save(&good, &path_str).unwrap();
        // Save again — now config.toml.bak holds the prior good copy (555), live = 777.
        let mut good2: Config = toml::from_str(SAMPLE).unwrap();
        good2.monitoring.decision_interval_secs = 777;
        save(&good2, &path_str).unwrap();

        // Corrupt the live file (truncated TOML).
        fs::write(&path, "[api]\nprovider = \"anth").unwrap();

        let loaded = load(&path_str).expect("load recovers from .bak");
        // Recovery returns the last-known-good backup (555), not the corrupt live file.
        assert_eq!(loaded.monitoring.decision_interval_secs, 555);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn screen_error_watch_defaults_on_and_an_older_tray_leaves_it_unchanged() {
        let mut cfg: Config = toml::from_str(SAMPLE).expect("sample loads");
        assert!(
            cfg.monitoring.watch_screen_errors,
            "on for a config written before it"
        );
        assert!(cfg.to_ui_settings().watch_screen_errors);
        let update = |watch| SettingsUpdate {
            provider: "claude_cli".into(),
            decision_interval_secs: 600,
            event_log_poll_interval_secs: 45,
            wmi_poll_interval_secs: 300,
            confidence_threshold: 0.8,
            watch_screen_errors: watch,
            ..Default::default()
        };
        cfg.apply_update(update(Some(false))).expect("apply off");
        assert!(!cfg.monitoring.watch_screen_errors);
        cfg.apply_update(update(None)).expect("older tray update");
        assert!(
            !cfg.monitoring.watch_screen_errors,
            "None keeps the stored choice"
        );
        let reparsed: Config =
            toml::from_str(&toml::to_string_pretty(&cfg).expect("serialize")).expect("reparse");
        assert!(!reparsed.monitoring.watch_screen_errors);
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
    fn loaded_numeric_settings_are_bounded_before_runtime_arithmetic() {
        let dir = std::env::temp_dir().join(format!("eir-cfg-schedule-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let source = SAMPLE
            .replace(
                r#"event_log_channels = ["System"]"#,
                r#"event_log_channels = [" System ", "system", "bad\nchannel"]"#,
            )
            .replace(
                "event_log_poll_interval_secs = 30",
                &format!("event_log_poll_interval_secs = {}", i64::MAX),
            )
            .replace(
                "wmi_poll_interval_secs = 300",
                &format!("wmi_poll_interval_secs = {}", i64::MAX),
            )
            .replace(
                "decision_interval_secs = 600",
                &format!("decision_interval_secs = {}", i64::MAX),
            )
            .replace("[persistence]", "confidence_threshold = nan\n[persistence]");
        fs::write(
            &path,
            format!(
                "{source}\n[updater]\nschedule_interval_secs = {}\n\
                 max_attempts_per_app = {}\nmax_apps_per_run = {}\nmax_installer_mb = {}\n",
                i64::MAX,
                u32::MAX,
                u32::MAX,
                i64::MAX,
            ),
        )
        .expect("config");
        let cfg = load(&path.to_string_lossy()).expect("load");
        assert!(
            cfg.updater.schedule_interval_secs
                <= crate::updater::config::MAX_SCHEDULE_INTERVAL_SECS
        );
        assert!(cfg.updater.max_attempts_per_app <= crate::updater::config::MAX_ATTEMPTS_PER_APP);
        assert!(cfg.updater.max_apps_per_run <= crate::updater::config::MAX_APPS_PER_RUN);
        assert!(cfg.updater.max_installer_mb <= crate::updater::config::MAX_INSTALLER_MB);
        assert!(cfg.monitoring.decision_interval_secs <= MAX_MONITORING_INTERVAL_SECS);
        assert!(cfg.monitoring.event_log_poll_interval_secs <= MAX_MONITORING_INTERVAL_SECS);
        assert!(cfg.monitoring.wmi_poll_interval_secs <= MAX_MONITORING_INTERVAL_SECS);
        assert!(cfg.monitoring.confidence_threshold.is_finite());
        assert_eq!(cfg.monitoring.event_log_channels, ["System"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_open_router_provider_alias_still_parses() {
        // Older configs serialized OpenRouter as "open_router"; must still load.
        let toml = SAMPLE.replace("\"opencode_cli\"", "\"open_router\"");
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.api.provider.as_str(), "opencode_cli");
    }

    #[test]
    fn removed_provider_aliases_to_claude_cli() {
        // A config written by an older build (openai_compatible / anthropic)
        // aliases to claude_cli; unknown keys are ignored by the toml loader.
        let src = SAMPLE.replace(
            "provider = \"opencode_cli\"",
            "provider = \"openai_compatible\"\nbase_url = \"http://localhost:8080/v1\"\napi_key = \"not-needed\"",
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        assert_eq!(cfg.api.provider.as_str(), "claude_cli");
    }

    #[test]
    fn claude_cli_provider_round_trips_with_no_key_or_model() {
        // The subscription path: a v0.16-style config (claude_cli, blank model,
        // optional profile/binary hints) parses as its own provider again and
        // survives a save/load cycle.
        let src = SAMPLE.replace(
            "provider = \"opencode_cli\"",
            "provider = \"claude_cli\"\nuser_profile = 'C:\\Users\\X'\nclaude_cli_path = 'C:\\Users\\X\\.local\\bin\\claude.exe'",
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        assert_eq!(cfg.api.provider, ApiProvider::Claude);
        assert_eq!(cfg.api.user_profile.as_deref(), Some("C:\\Users\\X"));

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::Claude);
        assert_eq!(
            reparsed.api.claude_cli_path.as_deref(),
            Some("C:\\Users\\X\\.local\\bin\\claude.exe")
        );

        // And the UI settings projection reports the provider token the
        // Settings dropdown uses.
        assert_eq!(reparsed.to_ui_settings().provider, "claude_cli");
    }

    #[test]
    fn codex_cli_provider_round_trips_with_no_key_or_model() {
        let src = SAMPLE.replace(
            "provider = \"opencode_cli\"",
            "provider = \"codex_cli\"\ncodex_cli_path = 'C:\\tools\\codex.exe'",
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        assert_eq!(cfg.api.provider, ApiProvider::Codex);

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::Codex);
        assert_eq!(
            reparsed.api.codex_cli_path.as_deref(),
            Some("C:\\tools\\codex.exe")
        );
        assert_eq!(reparsed.to_ui_settings().provider, "codex_cli");
    }

    #[test]
    fn changing_provider_drops_the_old_escalation_model() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.advisor.escalation_model = "claude-opus-4-8".into();
        cfg.apply_update(SettingsUpdate {
            provider: "codex_cli".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(cfg.advisor.escalation_model.is_empty());
    }

    #[test]
    fn kilocode_provider_alias_loads_as_opencode_cli() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "kilocode".into(),
            model: "ollama/llama3.2".into(),
            ..Default::default()
        })
        .unwrap();
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::OpenCode);
    }

    #[test]
    fn opencode_cli_provider_round_trips_with_overrides() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "opencode_cli".into(),
            model: "ollama/llama3.2".into(),
            opencode_cli_user_profile: Some(r"C:\Users\You".into()),
            opencode_cli_path: Some(r"C:\Users\You\.local\bin\opencode.exe".into()),
            ..Default::default()
        })
        .unwrap();
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::OpenCode);
        assert_eq!(reparsed.api.model, "ollama/llama3.2");
        assert_eq!(
            reparsed.api.opencode_cli_user_profile.as_deref(),
            Some(r"C:\Users\You")
        );
        assert_eq!(
            reparsed.api.opencode_cli_path.as_deref(),
            Some(r"C:\Users\You\.local\bin\opencode.exe")
        );
        let view = reparsed.to_ui_settings();
        assert!(view.opencode_cli_user_profile_set);
        assert!(view.opencode_cli_path_set);
        assert!(!view.kilo_cli_path_set);
        assert!(!view.openrouter_key_set);
    }

    #[test]
    fn cursor_cli_provider_round_trips_with_overrides() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.apply_update(SettingsUpdate {
            provider: "cursor_cli".into(),
            model: "auto".into(),
            cursor_cli_path: Some(r"C:\Users\You\.local\bin\agent.cmd".into()),
            cursor_cli_user_profile: Some(r"C:\Users\You".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.api.provider, ApiProvider::Cursor);
        let view = cfg.to_ui_settings();
        assert_eq!(view.provider, "cursor_cli");
        assert!(view.cursor_cli_path_set);
        assert!(view.cursor_cli_user_profile_set);
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.api.provider, ApiProvider::Cursor);
        assert_eq!(
            reparsed.api.cursor_cli_path.as_deref(),
            Some(r"C:\Users\You\.local\bin\agent.cmd")
        );
    }

    #[test]
    fn opencode_cli_blank_overrides_keep_stored_values() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.api.opencode_cli_user_profile = Some(r"C:\Users\Old".into());
        cfg.api.opencode_cli_path = Some(r"C:\old\opencode.exe".into());
        cfg.apply_update(SettingsUpdate {
            provider: "opencode_cli".into(),
            opencode_cli_user_profile: Some(String::new()),
            opencode_cli_path: Some(String::new()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            cfg.api.opencode_cli_user_profile.as_deref(),
            Some(r"C:\Users\Old")
        );
        assert_eq!(
            cfg.api.opencode_cli_path.as_deref(),
            Some(r"C:\old\opencode.exe")
        );
    }
}
