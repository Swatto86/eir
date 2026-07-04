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
    /// Autonomous-updater status (None until the service reports it). `#[serde(default)]`
    /// keeps an older payload (without this field) decodable.
    #[serde(default)]
    pub updater: Option<UpdaterStatus>,
    /// Advisor-mode status (self-tuning reasoning effort/model). `#[serde(default)]`
    /// for backward-compatible decode.
    #[serde(default)]
    pub advisor: Option<AdvisorStatus>,
    /// What Eir has learned about this machine (self-improvement), for the UI's
    /// transparency card. `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub learned_facts: Vec<LearnedFactView>,
    /// The latest weekly plain-English health digest, if one has been generated.
    /// `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub digest: Option<DigestView>,
    /// Recent CPU/memory/disk history for the dashboard timeline (chronological).
    /// `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub history: Vec<MetricPoint>,
    /// "Ask Eir" free-text Q&A state (None until the first question). `#[serde(default)]`
    /// keeps an older payload decodable.
    #[serde(default)]
    pub ask: Option<AskStatus>,
    /// On-demand disk-space scan results (None until the first scan). `#[serde(default)]`
    /// keeps an older payload decodable.
    #[serde(default)]
    pub disk_insights: Option<DiskInsightsView>,
    /// On-demand startup-entry scan results (None until the first scan).
    /// `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub startup: Option<StartupView>,
}

/// One point of the dashboard resource timeline. Percentages, unix-seconds `at`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MetricPoint {
    pub at: i64,
    pub cpu: f32,
    pub memory: f32,
    pub disk: f32,
}

/// "Ask Eir" state: whether an answer is being generated, the last error, and the
/// recent question/answer history (newest first).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AskStatus {
    pub running: bool,
    pub error: Option<String>,
    pub entries: Vec<AskEntry>,
}

/// One answered question. `answer` is diagnostic prose — nothing is parsed or
/// executed from it.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AskEntry {
    pub question: String,
    pub answer: String,
    /// Unix seconds when answered.
    pub at: i64,
}

/// Results of an on-demand disk-space scan, rendered in the Disk view.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DiskInsightsView {
    pub running: bool,
    /// Unix seconds of the last completed scan (0 = never).
    pub scanned_at: i64,
    pub error: Option<String>,
    pub entries: Vec<DiskEntryView>,
}

/// One space consumer found by the disk scan. `note`/`category` are deterministic
/// (never AI-authored), so the user can trust them. `cleanable` means the entry maps
/// to a safe fix-action offered behind the normal policy gate.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DiskEntryView {
    /// Stable id (hash of the path) — the key the UI sends back to clean it.
    pub id: String,
    pub path: String,
    pub size_bytes: u64,
    pub category: String,
    pub note: String,
    pub cleanable: bool,
}

/// Results of an on-demand startup-entry scan, rendered in the Startup view.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StartupView {
    pub running: bool,
    /// Unix seconds of the last completed scan (0 = never).
    pub scanned_at: i64,
    pub error: Option<String>,
    pub entries: Vec<StartupEntryView>,
}

/// One logon startup entry. `verdict`/`note` are AI-advisory (empty when the AI is
/// unconfigured — the deterministic listing is useful on its own); they trigger
/// nothing. `location` is a closed-set selector (never a raw registry path).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StartupEntryView {
    /// Stable id — the key the UI sends back to enable/disable it.
    pub id: String,
    pub name: String,
    pub command: String,
    /// Where it launches from: "hkcu_run" | "hklm_run" | "startup_folder" |
    /// "common_startup_folder" | "scheduled_task". A selector, not a path.
    pub location: String,
    pub enabled: bool,
    /// AI classification: "keep" | "optional" | "unnecessary" (empty if unconfigured).
    pub verdict: String,
    /// One-line plain-English "what this is" (empty if unconfigured).
    pub note: String,
}

/// The latest weekly health digest, rendered on the dashboard.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DigestView {
    pub text: String,
    /// Unix seconds when it was generated.
    pub generated_at: i64,
}

/// One learned fact, rendered in the UI's "What Eir has learned" card.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LearnedFactView {
    /// `learned_facts.id` — the key for Pin / Disable / Forget.
    pub id: i64,
    /// Plain-English summary of what was learned and its effect.
    pub summary: String,
    /// The supporting evidence ("3 timed-out cycles, 0 successes in 30d").
    pub detail: String,
    /// active | expired | user_pinned | user_disabled.
    pub status: String,
    /// detector | ai_labelled.
    pub source: String,
}

/// Advisor-mode status: whether the last analysis escalated to deeper reasoning, and
/// the day's escalation spend.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdvisorStatus {
    pub enabled: bool,
    /// Whether the most recent analysis cycle escalated.
    pub escalated: bool,
    /// The model escalated to (empty if effort-only or not escalated).
    pub escalation_model: String,
    /// Why it escalated ("the agent flagged ambiguity", "confidence was low").
    pub reason: String,
    /// Escalation AI spend so far today (USD).
    pub spent_today_usd: f64,
    /// Editable advisor settings, surfaced for the Settings panel.
    pub settings: AdvisorSettingsView,
}

/// Advisor settings shown in the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdvisorSettingsView {
    pub enabled: bool,
    pub escalation_model: String,
    pub escalation_effort: String,
    pub low_confidence_threshold: f32,
    pub budget_usd_per_day: f64,
}

/// An advisor-settings change from the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdvisorSettingsUpdate {
    pub enabled: bool,
    pub escalation_model: String,
    pub escalation_effort: String,
    pub low_confidence_threshold: f32,
    pub budget_usd_per_day: f64,
}

/// Live status of the autonomous app updater, rendered by the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdaterStatus {
    pub enabled: bool,
    /// True while a cycle is in progress.
    pub running: bool,
    /// Coarse phase text ("idle", "checking…", "updating apps…").
    pub phase: String,
    /// Unix seconds of the last completed cycle (0 = never).
    pub last_run: i64,
    /// Unix seconds the next scheduled cycle is due (0 = not scheduled).
    pub next_run: i64,
    /// AI cost (USD) of the last cycle.
    pub last_cost_usd: f64,
    /// Notes from the last cycle (truncation, check failures).
    pub notes: Vec<String>,
    /// Per-app result of the last cycle.
    pub apps: Vec<UpdaterAppRow>,
    /// Recent attempt history (newest first).
    pub recent: Vec<UpdateAttemptRow>,
    /// Editable updater settings, surfaced for the Settings panel.
    pub settings: UpdaterSettingsView,
}

/// One app's result in the last update cycle.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdaterAppRow {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    /// The method that ultimately handled it (or the last one tried).
    pub method: String,
    /// "verified" | "installed" | "failed" | "skipped".
    pub state: String,
    pub detail: String,
    /// Authenticode result for a native install (empty otherwise).
    pub signature: String,
    /// True when the user has this app on the updater ignore list. Set live by the
    /// SetAppIgnore handler so the UI can reflect the toggle immediately (a fresh
    /// cycle simply drops ignored apps from the list). `#[serde(default)]` for
    /// forward/backward wire-compat with a peer that predates the field.
    #[serde(default)]
    pub ignored: bool,
}

/// One persisted attempt, for the history view.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdateAttemptRow {
    pub name: String,
    pub method: String,
    pub success: bool,
    pub detail: String,
    /// Unix seconds.
    pub at: i64,
}

/// Updater settings shown in the UI (no secrets).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdaterSettingsView {
    pub enabled: bool,
    pub schedule_interval_secs: u64,
    pub methods: Vec<String>,
    pub native_enabled: bool,
    pub native_signature_policy: String,
}

/// An updater-settings change from the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdaterSettingsUpdate {
    pub enabled: bool,
    pub schedule_interval_secs: u64,
    pub methods: Vec<String>,
    pub native_enabled: bool,
    pub native_signature_policy: String,
}

/// Current settings shown in the UI. Secrets are never sent — only whether they
/// are set, so the UI can show "configured" without exposing the value.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UiSettings {
    pub provider: String,
    pub model: String,
    /// Model used for the app-update AI check (web search where the provider
    /// supports it). Empty = a provider-appropriate default.
    pub update_check_model: String,
    /// Reasoning effort: one of low, medium, high, xhigh, max. Empty = the
    /// provider default. Maps to `output_config.effort` (Anthropic) or
    /// `reasoning.effort` (OpenRouter / Kilo Code).
    #[serde(default)]
    pub effort: String,
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
    /// Whether a kilo_cli user-profile override is configured (the Windows
    /// profile whose logged-in Kilo session the LocalSystem service borrows).
    /// `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub kilo_cli_user_profile_set: bool,
    /// Whether a kilo_cli binary path override is configured. Same default
    /// rationale as `kilo_cli_user_profile_set`.
    #[serde(default)]
    pub kilo_cli_path_set: bool,
    /// Deprecated (pre-0.17 OpenAI-compatible provider). Always empty/false —
    /// kept on the wire so a not-yet-updated tray app, which requires these
    /// fields, can still decode the payload during an update's skew window.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
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
    pub openrouter_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    /// kilo_cli: the Windows user profile whose logged-in Kilo session the
    /// LocalSystem service borrows (e.g. `C:\Users\You`). Blank = auto-detect
    /// by scanning `C:\Users` for `.local\share\kilo\auth.json`. `None` = unchanged.
    #[serde(default)]
    pub kilo_cli_user_profile: Option<String>,
    /// kilo_cli: path to the `kilo` binary. Blank = auto-detect on PATH.
    /// `None` = unchanged.
    #[serde(default)]
    pub kilo_cli_path: Option<String>,
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
    /// When this was a registry reset whose prior value was captured, the id of the
    /// undo record — the UI shows a one-click "Undo" for it. `None` for any other
    /// execution (or once already undone). `#[serde(default)]` for wire-compat.
    #[serde(default)]
    pub undo_id: Option<i64>,
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
    Approve {
        id: u64,
        approved: bool,
    },
    TogglePause,
    UpdateSettings(Box<SettingsUpdate>),
    /// Clear the in-memory Recent Problems list.
    ClearProblems,
    /// Clear the in-memory Recent Executions list.
    ClearExecutions,
    /// Run an update cycle now (on demand).
    RunUpdatesNow,
    /// Clear the app-update output: the last cycle's results and the persisted
    /// attempt history.
    ClearUpdateHistory,
    /// Override a learned fact: op is "pin" (keep), "disable" (ignore), or "forget"
    /// (delete). User overrides always win over the detector.
    SetLearnedFact {
        id: i64,
        op: String,
    },
    /// Apply updater settings live (no service restart).
    UpdateUpdaterSettings(Box<UpdaterSettingsUpdate>),
    /// Ignore/un-ignore an app, or set a per-app note for the AI.
    SetAppIgnore {
        id: String,
        ignore: bool,
        note: String,
    },
    /// Apply advisor settings live (no service restart).
    SetAdvisorSettings(Box<AdvisorSettingsUpdate>),
    /// Undo a completed registry reset, restoring the captured prior value. `id` is
    /// the `ExecutionSummary.undo_id`.
    UndoRegistry {
        id: i64,
    },
    /// Ask Eir a free-text question, answered with live system context. The answer is
    /// diagnostic prose only — nothing is parsed or executed from it.
    AskEir {
        question: String,
    },
    /// Run an on-demand disk-space scan.
    ScanDisk,
    /// Clean one disk-scan entry by its id. The service maps the id to a safe
    /// fix-action from its own last scan and routes it through the normal policy gate.
    CleanDiskEntry {
        id: String,
    },
    /// Run an on-demand startup-entry scan.
    ScanStartup,
    /// Enable or disable one startup entry by its id (Task-Manager-style, reversible).
    /// The service maps the id to an entry from its own last scan.
    SetStartupEntry {
        id: String,
        enable: bool,
    },
}
