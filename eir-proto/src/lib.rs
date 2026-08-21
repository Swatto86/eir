use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\EirSvc";
pub const PROTOCOL_VERSION: u32 = 2;
pub const CAP_COMMAND_RESULTS: &str = "command_results";
pub const CAP_PROVIDER_TEST: &str = "provider_test";
pub const CAP_TARGETED_UPDATE_RETRY: &str = "targeted_update_retry";

/// Everything this service build supports, for every StatusPayload it sends —
/// the startup seed, the degraded projection, and the decision loop's snapshot.
/// One list: three hand-maintained copies is how the startup seed came to omit
/// CAP_TARGETED_UPDATE_RETRY and tell the UI a supported feature was missing.
pub fn service_capabilities() -> Vec<String> {
    vec![
        CAP_COMMAND_RESULTS.to_string(),
        CAP_PROVIDER_TEST.to_string(),
        CAP_TARGETED_UPDATE_RETRY.to_string(),
    ]
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StatusPayload {
    /// Wire contract version and optional features supported by the service.
    /// Both default for a tray temporarily connected to an older service.
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub status: String,
    pub paused: bool,
    pub cpu: f32,
    pub memory: f32,
    pub disk: f32,
    pub failed_services: Vec<String>,
    pub last_analysis: String,
    /// Unix seconds the last completed AI analysis finished (0 = none yet this
    /// service run). `#[serde(default)]` keeps an older payload decodable.
    #[serde(default)]
    pub last_analysis_at: i64,
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
    /// User-set Ignore / Always Approve preferences for specific fix actions.
    /// Surfaced so they can be reversed. `#[serde(default)]` for older peers.
    #[serde(default)]
    pub action_preferences: Vec<ActionPreferenceView>,
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
    /// True while Game Mode is active — a fullscreen game/app is running (auto-detected
    /// by the tray) or the user toggled it on, so Eir suppresses its own disruptive
    /// background work (updater, digest). `#[serde(default)]` keeps an older payload
    /// decodable.
    #[serde(default)]
    pub gaming: bool,
    /// Service binary version, surfaced in the About dialog so users can verify the
    /// running service matches the installed UI. `#[serde(default)]` keeps an older
    /// payload decodable.
    #[serde(default)]
    pub svc_version: Option<String>,
    /// Unix seconds when the system-signal snapshot was collected (0 = unknown).
    #[serde(default)]
    pub signals_at: i64,
    /// Collector tokens whose latest value is unavailable or degraded.
    #[serde(default)]
    pub signal_errors: Vec<String>,
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
    /// Short labels of the attachments this question carried (for the history card).
    #[serde(default)]
    pub attachments: Vec<String>,
}

/// A file/image/folder-file attachment for an Ask question, already read and bounded by
/// the tray (in the user's session — the service never opens an attacker-named path).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AskAttachment {
    /// Display name (filename, or `folder/file.txt` for a folder pick).
    pub name: String,
    /// `"text"` or `"image"`.
    pub kind: String,
    /// For `text`: the (bounded, UTF-8) file text. For `image`: base64-encoded bytes.
    pub content: String,
    /// For `image`: the media type (`image/jpeg` / `image/png`). Empty for text.
    #[serde(default)]
    pub media_type: String,
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
    /// Where it launches from: "hkcu_run" | "hklm_run" | "hklm_run32" |
    /// "startup_folder" | "common_startup_folder" | "run_once" | "policies_run" |
    /// "winlogon" | "scheduled_task" | "service". A selector, not a path.
    pub location: String,
    pub enabled: bool,
    /// AI classification: "keep" | "optional" | "unnecessary" (empty if unconfigured).
    pub verdict: String,
    /// Plain-English "what this is and where it likely came from" (empty if
    /// unconfigured).
    pub note: String,
    /// Authenticode signer CN of the launched binary, falling back to the file's
    /// CompanyName; empty when unsigned/unresolvable. Deterministic, never
    /// AI-authored. `#[serde(default)]` for wire-compat.
    #[serde(default)]
    pub signer: String,
    /// True for locations Eir lists for awareness but offers no toggle for
    /// (RunOnce, policy keys, Winlogon, services). `#[serde(default)]` for
    /// wire-compat (older services' entries were all toggleable and default false).
    #[serde(default)]
    pub report_only: bool,
    /// True when the binary is signed by Microsoft — the UI's "Hide Windows
    /// entries" filter keys off this. Deterministic. `#[serde(default)]`.
    #[serde(default)]
    pub microsoft: bool,
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

/// One durable Ignore / Always Approve preference for a semantic fix action.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ActionPreferenceView {
    /// `FixAction::dedup_key` — stable across regenerable AI parameters.
    pub action_key: String,
    /// `ignore` | `always_approve`.
    pub preference: String,
    /// Human-readable description of what the action does.
    pub summary: String,
    /// Concrete target (process, path, service, …), when known.
    #[serde(default)]
    pub target: String,
    /// Unix seconds when the preference was set.
    #[serde(default)]
    pub created_at: i64,
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
}

/// An advisor-settings change from the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdvisorSettingsUpdate {
    pub enabled: bool,
    pub escalation_model: String,
    pub escalation_effort: String,
    pub low_confidence_threshold: f32,
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
    /// Unix seconds of the last completed cycle without failures (0 = never).
    #[serde(default)]
    pub last_clean_run: i64,
    /// Unix seconds the next scheduled cycle is due (0 = not scheduled).
    pub next_run: i64,
    /// AI cost (USD) of the last cycle.
    pub last_cost_usd: f64,
    /// Notes from the last cycle (truncation, check failures).
    pub notes: Vec<String>,
    /// Persistent per-app guidance the user can read, edit, or delete even when
    /// that app is absent from the latest cycle.
    #[serde(default)]
    pub app_notes: Vec<UpdaterAppNote>,
    /// Per-app result of the last cycle.
    pub apps: Vec<UpdaterAppRow>,
    /// Recent attempt history (newest first).
    pub recent: Vec<UpdateAttemptRow>,
    /// Editable updater settings, surfaced for the Settings panel.
    pub settings: UpdaterSettingsView,
}

/// One persisted instruction included in future AI checks for this app.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdaterAppNote {
    pub id: String,
    pub note: String,
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
    /// Persisted user guidance included in future AI update checks and native
    /// installer searches. `#[serde(default)]` keeps older peers compatible.
    #[serde(default)]
    pub note: String,
}

/// One persisted attempt, for the history view.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdateAttemptRow {
    pub name: String,
    pub method: String,
    pub success: bool,
    pub detail: String,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub to_version: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
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
    #[serde(default)]
    pub ignored: Vec<String>,
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
    /// provider default. Maps to the equivalent Anthropic, OpenRouter,
    /// Claude CLI, Codex CLI, or Kilo CLI reasoning control.
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
    /// ollama: OpenAI-compatible API root (e.g. `http://127.0.0.1:11434/v1`).
    #[serde(default)]
    pub ollama_base_url: String,
    /// Whether an Ollama cloud API key is configured (for web search).
    #[serde(default)]
    pub ollama_key_set: bool,
    /// Deprecated (pre-0.17 OpenAI-compatible provider). Always empty/false —
    /// kept on the wire so a not-yet-updated tray app, which requires these
    /// fields, can still decode the payload during an update's skew window.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key_set: bool,
    /// Auto-enable Game Mode when a fullscreen game is detected (the tray reads this to
    /// decide whether to run the detector). `#[serde(default)]` keeps an older payload
    /// decodable (defaults to false there, but the service default is true).
    #[serde(default)]
    pub game_mode_auto: bool,
    /// Switch to the High Performance power plan while Game Mode is active. `#[serde(default)]`.
    #[serde(default)]
    pub game_mode_power_boost: bool,
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
    /// ollama: OpenAI-compatible API root. Blank keeps the stored URL.
    #[serde(default)]
    pub ollama_base_url: String,
    /// ollama: cloud API key for web search (`ollama.com/settings/keys`).
    /// `None` = unchanged.
    #[serde(default)]
    pub ollama_api_key: Option<String>,
    pub decision_interval_secs: u64,
    pub event_log_poll_interval_secs: u64,
    pub wmi_poll_interval_secs: u64,
    pub event_log_channels: Vec<String>,
    pub log_directories: Vec<String>,
    #[serde(default)]
    pub confidence_threshold: f32,
    /// Auto-enable Game Mode during fullscreen games. `#[serde(default)]` keeps an older
    /// tray app's update decodable.
    #[serde(default)]
    pub game_mode_auto: bool,
    /// Switch to High Performance power plan during Game Mode. `#[serde(default)]`.
    #[serde(default)]
    pub game_mode_power_boost: bool,
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

/// Result of a command carrying a request id. `ok` means the service applied
/// the command, rather than merely accepting bytes from the pipe.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CommandResult {
    pub request_id: u64,
    pub ok: bool,
    pub message: String,
}

/// Messages sent FROM the service TO the UI.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceMsg {
    Status(Box<StatusPayload>),
    CommandResult(CommandResult),
}

/// A command plus an optional correlation id. Flattening preserves the original
/// top-level `UiMsg` wire shape; older services simply ignore `request_id`.
#[derive(Serialize, Deserialize, Debug)]
pub struct UiRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    #[serde(flatten)]
    pub command: UiMsg,
}

impl From<UiMsg> for UiRequest {
    fn from(command: UiMsg) -> Self {
        Self {
            request_id: None,
            command,
        }
    }
}

/// Messages sent FROM the UI TO the service.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiMsg {
    Approve {
        id: u64,
        approved: bool,
    },
    /// Remember a preference for the semantic action behind a pending approval.
    /// `preference` is `ignore` (dismiss and never re-queue) or `always_approve`
    /// (approve now and auto-approve matching future proposals). Reversible via
    /// [`ClearActionPreference`].
    SetActionPreference {
        id: u64,
        preference: String,
    },
    /// Remove an Ignore / Always Approve preference by its stable action key.
    ClearActionPreference {
        action_key: String,
    },
    TogglePause,
    UpdateSettings(Box<SettingsUpdate>),
    /// Clear the in-memory Recent Problems list.
    ClearProblems,
    /// Clear the in-memory Recent Executions list.
    ClearExecutions,
    /// Force an immediate live-status refresh (fast services rescan + re-settle), so a
    /// recovered failed service clears without waiting for the next poll/decision tick.
    RefreshStatus,
    /// Set Game Mode. `manual: false` is the tray's auto-detector — `on: true` extends a
    /// short lease re-asserted on a heartbeat, so a crashed tray auto-expires it. `manual:
    /// true` is the user's explicit toggle — a latch that persists until toggled off (no
    /// heartbeat needed). Gaming is active if the manual latch is set OR the lease is live.
    SetGaming {
        on: bool,
        manual: bool,
    },
    /// Run an update cycle now (on demand).
    RunUpdatesNow,
    /// Re-check and retry one failed app using the latest saved AI guidance.
    RetryAppUpdate {
        id: String,
    },
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
    /// Ignore/un-ignore an app, or set a per-app note for the AI. A blank `note`
    /// leaves any existing note unchanged (an ignore toggle carries an empty note);
    /// a non-empty note replaces it. Clearing a note is done in `config.toml`.
    SetAppIgnore {
        id: String,
        ignore: bool,
        note: String,
    },
    /// Save, replace, or clear persistent AI guidance for a detected app.
    SetAppNote {
        id: String,
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
    /// diagnostic prose only — nothing is parsed or executed from it. `attachments` are
    /// files/images/folder-files the tray already read + bounded (never a path).
    AskEir {
        question: String,
        #[serde(default)]
        attachments: Vec<AskAttachment>,
    },
    /// Clear the "Ask Eir" chat history and reset its context.
    ClearAsk,
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
    /// Verify the saved provider/model from the LocalSystem service context.
    TestProvider,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_flat_and_old_ui_msg_still_decodes_it() {
        let request = UiRequest {
            request_id: Some(42),
            command: UiMsg::TogglePause,
        };
        let json = serde_json::to_string(&request).expect("serialize request");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).expect("valid json"),
            serde_json::json!({"type": "toggle_pause", "request_id": 42})
        );
        assert!(matches!(
            serde_json::from_str::<UiMsg>(&json).expect("old service ignores request_id"),
            UiMsg::TogglePause
        ));
    }

    #[test]
    fn new_status_health_fields_default_for_old_service() {
        let status: StatusPayload = serde_json::from_str(
            r#"{"status":"Active","paused":false,"cpu":1.0,"memory":2.0,"disk":3.0,
                "failed_services":[],"last_analysis":"","recent_problems":[],
                "recent_executions":[],"error":null,"usage":null,"settings":null}"#,
        )
        .expect("old status decodes");
        assert_eq!(status.protocol_version, 0);
        assert!(status.capabilities.is_empty());
        assert_eq!(status.signals_at, 0);
        assert!(status.signal_errors.is_empty());
    }

    #[test]
    fn ignored_apps_default_for_an_older_updater_settings_payload() {
        let settings: UpdaterSettingsView = serde_json::from_str(
            r#"{"enabled":true,"schedule_interval_secs":86400,"methods":["winget"],
                "native_enabled":true,"native_signature_policy":"require_valid"}"#,
        )
        .expect("old updater settings decode");
        assert!(settings.ignored.is_empty());
    }

    #[test]
    fn command_result_has_stable_top_level_wire_shape() {
        let json = serde_json::to_value(ServiceMsg::CommandResult(CommandResult {
            request_id: 7,
            ok: false,
            message: "not allowed".to_string(),
        }))
        .expect("serialize result");
        assert_eq!(
            json,
            serde_json::json!({
                "type": "command_result",
                "request_id": 7,
                "ok": false,
                "message": "not allowed"
            })
        );
    }

    #[test]
    fn targeted_update_retry_has_stable_wire_shape() {
        let json = serde_json::to_value(UiMsg::RetryAppUpdate {
            id: "example app".to_string(),
        })
        .expect("serialize retry");
        assert_eq!(
            json,
            serde_json::json!({
                "type": "retry_app_update",
                "id": "example app"
            })
        );
    }

    #[test]
    fn action_preference_messages_have_stable_wire_shape() {
        let set = serde_json::to_value(UiMsg::SetActionPreference {
            id: 7,
            preference: "always_approve".to_string(),
        })
        .expect("serialize set");
        assert_eq!(
            set,
            serde_json::json!({
                "type": "set_action_preference",
                "id": 7,
                "preference": "always_approve"
            })
        );
        let clear = serde_json::to_value(UiMsg::ClearActionPreference {
            action_key: "process_kill|chrome".to_string(),
        })
        .expect("serialize clear");
        assert_eq!(
            clear,
            serde_json::json!({
                "type": "clear_action_preference",
                "action_key": "process_kill|chrome"
            })
        );
    }

    #[test]
    fn action_preferences_default_for_an_older_status_payload() {
        let status: StatusPayload = serde_json::from_value(serde_json::json!({
            "status": "Active",
            "paused": false,
            "cpu": 0.0,
            "memory": 0.0,
            "disk": 0.0,
            "failed_services": [],
            "last_analysis": "",
            "recent_problems": [],
            "recent_executions": [],
            "error": null,
            "usage": null,
            "settings": null
        }))
        .expect("old status still decodes");
        assert!(status.action_preferences.is_empty());
    }
}
