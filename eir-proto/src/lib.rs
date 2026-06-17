use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\EirSvc";

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
    /// Actions awaiting the user's decision. Persisted across cycles and service
    /// restarts, so an approval never expires out from under the user — it stays
    /// here until they Approve or Reject it.
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalInfo>,
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
    /// Claude model used for the on-demand "Other Updates" AI check (it needs
    /// web search). Empty = the CLI's default model.
    pub update_check_model: String,
    /// Reasoning effort for the Claude CLI provider (`--effort`): one of low,
    /// medium, high, xhigh, max. Empty = the CLI's default. Ignored by other providers.
    #[serde(default)]
    pub effort: String,
    pub base_url: String,
    pub decision_interval_secs: u64,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    /// Minimum AI confidence (0.0–1.0) for a whitelisted fix to auto-execute;
    /// anything below this is blocked.
    #[serde(default)]
    pub confidence_threshold: f32,
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
    pub update_check_model: String,
    #[serde(default)]
    pub effort: String,
    pub base_url: Option<String>,
    pub openrouter_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub api_key: Option<String>,
    pub decision_interval_secs: u64,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    #[serde(default)]
    pub confidence_threshold: f32,
}

/// Aggregated AI usage, surfaced in the UI so the user can see how much of
/// their subscription Eir is consuming. Cost is the equivalent pay-as-you-go
/// API cost reported by the provider — not a subscription charge.
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
    /// Debug rendering of the fix action (e.g. `FileDelete { path: "…" }`).
    pub action: String,
    /// Why this action needs approval (the policy verdict reason).
    pub reason: String,
    /// AI's account of what might break.
    pub side_effects: String,
    /// AI's instructions for reverting the change.
    pub undo_instructions: String,
    /// Deterministic, plain-English summary of exactly what executing this does —
    /// derived from the action type, not the AI, so it can be trusted.
    #[serde(default)]
    pub action_summary: String,
    /// The concrete target the action affects (a file path, service, process, …).
    #[serde(default)]
    pub target: String,
    /// Deterministic facts about the target gathered at proposal time (e.g. a
    /// file's size, last-modified date, and what kind of file it is). Empty when
    /// the action has no inspectable target. Multi-line.
    #[serde(default)]
    pub target_details: String,
    /// Whether the action can be undone after it runs. Surfaced so the user knows
    /// when they are approving a one-way door.
    #[serde(default)]
    pub reversible: bool,
    /// Unix timestamp (seconds) when the action was first proposed.
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProblemSummary {
    pub diagnosis: String,
    pub confidence: f32,
    pub action: String,
    pub blocked: bool,
    pub auto_executed: bool,
    /// Why it was blocked or held for approval (shown in the UI). None when it
    /// ran or needs no explanation.
    #[serde(default)]
    pub reason: Option<String>,
    /// Unix timestamp (seconds) when this entry was recorded; 0 if unknown.
    #[serde(default)]
    pub at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecutionSummary {
    pub action: String,
    pub success: bool,
    pub preview: String,
    /// Unix timestamp (seconds) when this execution ran; 0 if unknown.
    #[serde(default)]
    pub at: i64,
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
    /// Clear the in-memory Recent Problems list.
    ClearProblems,
    /// Clear the in-memory Recent Executions list.
    ClearExecutions,
}
