use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\SentrySvc";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StatusPayload {
    pub status: String,
    pub paused: bool,
    pub cpu: f32,
    pub memory: f32,
    pub disk: f32,
    pub failed_services: Vec<String>,
    pub last_analysis: String,
    pub recent_problems: Vec<ProblemSummary>,
    pub recent_executions: Vec<ExecutionSummary>,
    pub pending_approval: Option<ApprovalInfo>,
    pub error: Option<String>,
    /// AI usage totals (recorded when the provider reports usage); None if unavailable.
    pub usage: Option<UsageSummary>,
    /// Current configuration, surfaced so the UI can display and edit it.
    pub settings: Option<UiSettings>,
}

/// Current settings shown in the UI. Secrets are never sent — only whether they
/// are set, so the UI can show "configured" without exposing the value.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UiSettings {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub decision_interval_secs: u64,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    pub openrouter_key_set: bool,
    pub anthropic_key_set: bool,
    pub api_key_set: bool,
}

/// A settings change from the UI. Secret fields are `None` to mean "unchanged";
/// a non-empty value replaces the stored secret.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SettingsUpdate {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub openrouter_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub api_key: Option<String>,
    pub decision_interval_secs: u64,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
}

/// Aggregated Claude usage, surfaced in the UI so the user can see how much of
/// their subscription Sentry is consuming. Cost is the equivalent pay-as-you-go
/// API cost reported by the CLI — not a subscription charge.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UsageSummary {
    pub calls_today: u64,
    pub calls_week: u64,
    pub tokens_today: u64,
    pub tokens_week: u64,
    pub cost_today_usd: f64,
    pub cost_week_usd: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApprovalInfo {
    pub id: u64,
    pub diagnosis: String,
    pub root_cause: String,
    pub confidence: f32,
    pub action: String,
    pub reason: String,
    pub side_effects: String,
    pub undo_instructions: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProblemSummary {
    pub diagnosis: String,
    pub confidence: f32,
    pub action: String,
    pub blocked: bool,
    pub auto_executed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecutionSummary {
    pub action: String,
    pub success: bool,
    pub preview: String,
}

/// Messages sent FROM the service TO the UI.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceMsg {
    Status(StatusPayload),
}

/// Messages sent FROM the UI TO the service.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiMsg {
    Approve { id: u64, approved: bool },
    TogglePause,
    UpdateSettings(Box<SettingsUpdate>),
}
