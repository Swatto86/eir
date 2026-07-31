use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SignalSnapshot {
    pub timestamp: DateTime<Utc>,
    pub event_log: Vec<EventLogEntry>,
    pub file_changes: Vec<FileChange>,
    pub system_state: SystemState,
    pub decision_history: Vec<PastDecision>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EventLogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub source: String,
    pub message: String,
    pub event_id: u32,
}

impl EventLogEntry {
    /// Worth analysing — the single definition shared by the actionable
    /// fingerprint and the reactive trigger, so the two can't drift.
    pub fn is_actionable(&self) -> bool {
        self.level == "Error" || self.level == "Warning"
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: String,
    pub size_bytes: u64,
    pub timestamp: DateTime<Utc>,
    /// Structured diagnostic info extracted from the file's content.
    pub log_event: Option<LogEvent>,
}

/// Parsed diagnostic information extracted from a log file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LogEvent {
    /// Program that wrote this log, inferred from the file path.
    pub program: String,
    pub log_path: String,
    /// Highest severity seen: "FATAL", "ERROR", "WARN", or "INFO".
    pub severity: String,
    /// Up to 5 error excerpts, each with 1 line of context before and 2 after.
    pub error_snippets: Vec<String>,
    /// A capped raw excerpt of the file's actual content, so the AI can judge the
    /// finding in context (e.g. tell a benign `"error"` field in a JSON cache from
    /// genuine corruption) instead of reasoning from keyword-matched lines alone.
    #[serde(default)]
    pub content_excerpt: String,
}

impl LogEvent {
    /// Worth analysing — shared by the actionable fingerprint and the
    /// file-watch reactive trigger.
    pub fn is_actionable(&self) -> bool {
        self.severity != "INFO" || !self.error_snippets.is_empty()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct SystemState {
    /// Unix seconds when this snapshot finished collecting.
    #[serde(default)]
    pub collected_at: i64,
    /// Collector tokens whose values could not be refreshed.
    #[serde(default)]
    pub collector_errors: Vec<String>,
    pub uptime_secs: u64,
    pub cpu_usage_percent: f32,
    pub memory_usage_percent: f32,
    pub memory_available_gb: f32,
    pub disk_usage_percent: f32,
    pub disk_free_gb: f32,
    pub running_services_count: usize,
    pub failed_services: Vec<String>,
    pub network_interfaces: Vec<NetworkInterface>,
    pub network_errors: u32,
    pub disk_health: String,
    pub windows_update_status: String,
    /// Windows security posture (firewall + Defender). Defaulted so an older
    /// snapshot without the field still deserialises.
    #[serde(default)]
    pub security: SecurityPosture,
}

impl SystemState {
    /// Stable string parts for each actionable fault: failed services, firewall
    /// profiles explicitly off, and Defender faults while Defender is the active
    /// AV. The single definition shared by the actionable fingerprint and the
    /// WMI reactive trigger, so the two can't drift. `None`/healthy states are
    /// excluded so a secure, healthy machine produces no parts.
    pub fn fault_parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = self
            .failed_services
            .iter()
            .map(|s| format!("S|{s}"))
            .collect();
        let fw = &self.security.firewall;
        for (name, on) in [
            ("domain", fw.domain),
            ("private", fw.private),
            ("public", fw.public),
        ] {
            if on == Some(false) {
                parts.push(format!("FW|{name}"));
            }
        }
        // Defender faults only matter when Defender is the active AV — if a
        // third-party AV has taken over (antivirus_enabled == false) its
        // passive state is normal.
        let def = &self.security.defender;
        if def.antivirus_enabled != Some(false) {
            if def.realtime_enabled == Some(false) {
                parts.push("DEF|realtime_off".into());
            }
            if def.signature_age_days.is_some_and(|d| d > 3) {
                parts.push("DEF|sig_stale".into());
            }
        }
        // A SMART disk health of Warning/Unhealthy is a real early warning worth an
        // analysis. "Healthy"/"unknown" (and the empty default) are not faults.
        let dh = self.disk_health.to_lowercase();
        if dh == "warning" || dh == "unhealthy" {
            parts.push(format!("DISK_HEALTH|{dh}"));
        }
        parts
    }
}

/// Security-relevant machine state the AI can diagnose and (with the matching
/// fix-actions) repair: firewall profiles and Windows Defender.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct SecurityPosture {
    pub firewall: FirewallStatus,
    pub defender: DefenderStatus,
}

/// Whether the Windows Firewall is on for each network profile. `true` = on.
/// `None` means the state could not be read (don't treat unknown as a fault).
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct FirewallStatus {
    pub domain: Option<bool>,
    pub private: Option<bool>,
    pub public: Option<bool>,
}

/// Windows Defender antivirus state, as reported by `Get-MpComputerStatus`.
/// All fields are optional: if Defender is absent or the query fails, they stay
/// `None` and the AI is told not to infer a fault from missing data.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct DefenderStatus {
    /// Real-time (on-access) protection is enabled.
    pub realtime_enabled: Option<bool>,
    /// The antivirus engine is running (false when a third-party AV has taken over).
    pub antivirus_enabled: Option<bool>,
    /// Age of the signature definitions in days (0 = updated today).
    pub signature_age_days: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub status: String,
    pub ipv4: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PastDecision {
    pub timestamp: DateTime<Utc>,
    pub diagnosis: String,
    pub confidence: f32,
    pub fix_proposed: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Problem {
    pub diagnosis: String,
    pub root_cause: String,
    pub confidence: f32,
    pub proposed_fix: serde_json::Value,
    pub reasoning: String,
    pub side_effects: String,
    pub undo_instructions: String,
}

const MAX_MODEL_PROBLEMS: usize = 5;
const MAX_MODEL_DECISION_TEXT_BYTES: usize = 16 * 1024;
const MAX_MODEL_ACTION_STRING_BYTES: usize = 16 * 1024;
const MAX_MODEL_ACTION_BYTES: usize = 64 * 1024;

impl Problem {
    pub fn parse_fix_action(&self) -> Option<FixAction> {
        serde_json::from_value(self.proposed_fix.clone()).ok()
    }
}

/// Matches Claude's proposed_fix `action` field values.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FixAction {
    // Phase 2
    ServiceRestart {
        service_name: String,
    },
    ServiceStop {
        service_name: String,
    },
    ServiceStart {
        service_name: String,
    },
    LogCleanup {
        path: String,
        days_old: u32,
    },
    DiskCleanup {
        target: String,
    },
    // serde's snake_case would yield "power_shell_diagnostic"; the prompt and
    // model use "powershell_diagnostic", so pin the tag explicitly.
    #[serde(rename = "powershell_diagnostic")]
    PowerShellDiagnostic {
        script: String,
    },
    // Phase 3
    TaskDisable {
        task_name: String,
    },
    TaskEnable {
        task_name: String,
    },
    RegistryReset {
        key_path: String,
        value_name: String,
        value_data: String,
    },
    NetworkDiagnostic {
        command: String,
    },
    // Phase 4
    DriverDisable {
        driver_name: String,
    },
    DriverEnable {
        driver_name: String,
    },
    SoftwareUninstall {
        package_name: String,
    },
    BcdEdit {
        element: String,
        value: String,
    },
    ProcessKill {
        process_name: String,
    },
    /// Delete a single file (e.g. corrupted cache, lock file, bad config).
    /// Never deletes directories. Blocked by the path blocklist in policy.toml.
    FileDelete {
        path: String,
    },
    // Phase 5 — security self-healing (safe, reversible)
    /// Re-enable the Windows Firewall for a profile: domain | private | public | all.
    FirewallEnable {
        profile: String,
    },
    /// Update Windows Defender's signature definitions (`Update-MpSignature`).
    /// Always safe — it only refreshes definitions.
    DefenderSignatureUpdate,
    /// Turn Windows Defender real-time (on-access) protection back on. Reversible,
    /// but security-sensitive, so it is approval-gated, not auto-run.
    DefenderRealtimeEnable,
    /// Run `sfc /scannow` to verify and repair protected system files. Long-running
    /// and writes to the component store, so it is approval-gated, never whitelisted.
    SfcScan,
    /// Run `DISM /Online /Cleanup-Image /RestoreHealth` to repair the Windows component
    /// store (often a prerequisite for SFC). Long-running, approval-gated, never
    /// whitelisted.
    DismRestoreHealth,
    /// Enable or disable a logon startup entry by writing the Windows "StartupApproved"
    /// flag — the same mechanism as Task Manager's Startup tab. Fully reversible (nothing
    /// is deleted). Created only by the user-initiated startup scan; the AI never proposes
    /// it (it's not in the prompt's action catalogue). Approval-gated (off the whitelist).
    StartupSet {
        /// The StartupApproved value name: the Run value name, or the `.lnk` filename.
        name: String,
        /// Closed-set selector for the approved key: `machine_run` | `machine_run32` |
        /// `user_run` | `user_startup_folder` | `common_startup_folder`. Never a raw
        /// registry path.
        location: String,
        /// User SID for the `user_*` locations (validated in the adapter), empty otherwise.
        hive: String,
        enable: bool,
    },
}

impl FixAction {
    /// Stable identity of the *issue* an action addresses, used to stop duplicate
    /// approval cards piling up across analysis runs. A persistent fault is re-proposed
    /// every cycle, and AI-generated parameters (a PowerShell script body, a registry
    /// value payload, a cleanup age) rarely come back byte-identical — so the exact
    /// Debug label cannot be the dedup key. This keys on the variant plus the target it
    /// acts on (case-folded, Windows names being case-insensitive) and deliberately
    /// excludes the regenerable parameters.
    pub fn dedup_key(&self) -> String {
        let (kind, target) = match self {
            FixAction::ServiceRestart { service_name } => ("service_restart", service_name),
            FixAction::ServiceStop { service_name } => ("service_stop", service_name),
            FixAction::ServiceStart { service_name } => ("service_start", service_name),
            // days_old excluded: cleaning the same path is the same issue.
            FixAction::LogCleanup { path, .. } => {
                return format!("log_cleanup|{}", normalized_windows_target(path));
            }
            FixAction::DiskCleanup { target } => ("disk_cleanup", target),
            // The script body is regenerated every analysis run, so it can't identify
            // the issue. Key on the variant alone: at most one SYSTEM-script card is
            // pending at a time — a different issue needing a script simply re-surfaces
            // on a later cycle once the user has dealt with the current card.
            FixAction::PowerShellDiagnostic { .. } => {
                return "powershell_diagnostic".to_string();
            }
            FixAction::TaskDisable { task_name } => {
                return format!("task_disable|{}", normalized_windows_target(task_name));
            }
            FixAction::TaskEnable { task_name } => {
                return format!("task_enable|{}", normalized_windows_target(task_name));
            }
            // value_data excluded: resetting the same value is the same issue.
            FixAction::RegistryReset {
                key_path,
                value_name,
                ..
            } => {
                return format!(
                    "registry_reset|{}|{}",
                    normalized_windows_target(key_path),
                    value_name.to_ascii_lowercase()
                );
            }
            FixAction::NetworkDiagnostic { command } => ("network_diagnostic", command),
            FixAction::DriverDisable { driver_name } => ("driver_disable", driver_name),
            FixAction::DriverEnable { driver_name } => ("driver_enable", driver_name),
            FixAction::SoftwareUninstall { package_name } => ("software_uninstall", package_name),
            // value excluded: changing the same boot element is the same issue.
            FixAction::BcdEdit { element, .. } => ("bcd_edit", element),
            FixAction::ProcessKill { process_name } => ("process_kill", process_name),
            FixAction::FileDelete { path } => {
                return format!("file_delete|{}", normalized_windows_target(path));
            }
            FixAction::FirewallEnable { profile } => ("firewall_enable", profile),
            FixAction::DefenderSignatureUpdate => return "defender_signature_update".to_string(),
            FixAction::DefenderRealtimeEnable => return "defender_realtime_enable".to_string(),
            FixAction::SfcScan => return "sfc_scan".to_string(),
            FixAction::DismRestoreHealth => return "dism_restore_health".to_string(),
            // enable is INCLUDED: an enable and a disable card for the same entry are
            // different user intents (user-initiated only), not analysis-loop spam.
            FixAction::StartupSet {
                name,
                location,
                hive,
                enable,
            } => {
                return format!(
                    "startup_set|{}|{}|{}|{}",
                    name.to_ascii_lowercase(),
                    location.to_ascii_lowercase(),
                    hive.to_ascii_lowercase(),
                    enable
                );
            }
        };
        format!("{kind}|{}", target.to_ascii_lowercase())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExecutionResult {
    pub action: String,
    pub success: bool,
    pub output: String,
    /// For a `RegistryReset`, the snapshot of the value BEFORE it was overwritten, so
    /// the change can be reverted with one click. `None` for every other action.
    #[serde(default)]
    pub undo: Option<RegistryUndo>,
}

/// Registry scalar types that can be snapshotted and restored without losing data.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum RegistryScalarKind {
    String,
    ExpandString,
    DWord,
    QWord,
}

fn normalized_windows_target(target: &str) -> String {
    crate::policy::normalize_path_lexical(target).join("\\")
}

impl RegistryScalarKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::ExpandString => "ExpandString",
            Self::DWord => "DWord",
            Self::QWord => "QWord",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "String" => Some(Self::String),
            "ExpandString" => Some(Self::ExpandString),
            "DWord" => Some(Self::DWord),
            "QWord" => Some(Self::QWord),
            _ => None,
        }
    }
}

/// The prior state of a registry value captured before a `RegistryReset`, enough to
/// restore its exact scalar type and data, or delete the value if it did not exist.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistryUndo {
    pub key_path: String,
    pub value_name: String,
    /// True if the value existed before we wrote it (restore = set back to prior_data);
    /// false if we created it (restore = delete the value).
    pub prior_existed: bool,
    #[serde(default)]
    pub prior_kind: Option<RegistryScalarKind>,
    pub prior_data: Option<String>,
    /// Exact value Eir wrote. Undo is allowed only while the live value still matches
    /// this type and data, so a later user/application change is never overwritten.
    #[serde(default)]
    pub applied_kind: Option<RegistryScalarKind>,
    #[serde(default)]
    pub applied_data: Option<String>,
}

/// A fix awaiting the user's decision. Carries everything needed to execute it
/// whenever the user gets around to approving it — and to record feedback once it
/// runs — so approval is no longer tied to a blocking timeout. Persisted in the
/// audit DB so it survives idle cycles and service restarts.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    /// What the UI shows; `info.id` is the audit-DB row id.
    pub info: eir_proto::ApprovalInfo,
    /// The exact action to run if approved.
    pub action: FixAction,
    /// Decision this fix came from, for linking the execution record.
    pub decision_id: i64,
    /// System state when proposed, used as the "before" baseline for feedback.
    pub baseline: SystemState,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeDecision {
    pub analysis: String,
    pub problems: Vec<Problem>,
    /// The model sets this when the signals look concerning but it can't confidently
    /// diagnose/fix them at the current reasoning level — the advisor-mode trigger to
    /// re-analyze at higher effort / a stronger model. `#[serde(default)]` so an older
    /// or terse response (without the field) still parses.
    #[serde(default)]
    pub needs_deeper_analysis: bool,
}

impl ClaudeDecision {
    /// Enforce the model's response contract before any output is logged, displayed, or
    /// routed to policy. Prose can be shortened safely; an oversized action is dropped
    /// whole because truncating a path, service name, script, or registry value could
    /// change the privileged operation.
    pub fn bound_model_output(&mut self) {
        truncate_utf8_bytes(&mut self.analysis, MAX_MODEL_DECISION_TEXT_BYTES);
        for problem in &mut self.problems {
            truncate_utf8_bytes(&mut problem.diagnosis, MAX_MODEL_DECISION_TEXT_BYTES);
            truncate_utf8_bytes(&mut problem.root_cause, MAX_MODEL_DECISION_TEXT_BYTES);
            truncate_utf8_bytes(&mut problem.reasoning, MAX_MODEL_DECISION_TEXT_BYTES);
            truncate_utf8_bytes(&mut problem.side_effects, MAX_MODEL_DECISION_TEXT_BYTES);
            truncate_utf8_bytes(
                &mut problem.undo_instructions,
                MAX_MODEL_DECISION_TEXT_BYTES,
            );
        }
        self.problems.retain(|problem| {
            json_strings_within(&problem.proposed_fix, MAX_MODEL_ACTION_STRING_BYTES)
                && serde_json::to_vec(&problem.proposed_fix)
                    .is_ok_and(|json| json.len() <= MAX_MODEL_ACTION_BYTES)
                && problem.parse_fix_action().is_some()
        });
        self.problems.truncate(MAX_MODEL_PROBLEMS);
    }
}

fn json_strings_within(value: &serde_json::Value, max_bytes: usize) -> bool {
    match value {
        serde_json::Value::String(value) => value.len() <= max_bytes,
        serde_json::Value::Array(values) => values
            .iter()
            .all(|value| json_strings_within(value, max_bytes)),
        serde_json::Value::Object(values) => values
            .iter()
            .all(|(key, value)| key.len() <= max_bytes && json_strings_within(value, max_bytes)),
        _ => true,
    }
}

pub(crate) fn truncate_utf8_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

/// Token + cost usage for a single AI call. Cost is provider-reported
/// (OpenRouter) or estimated from list pricing (Anthropic); 0 when unknown.
#[derive(Debug, Clone, Default)]
pub struct CallUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost_usd: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_problem(fix: serde_json::Value) -> Problem {
        Problem {
            diagnosis: "diagnosis".into(),
            root_cause: "root cause".into(),
            confidence: 0.9,
            proposed_fix: fix,
            reasoning: "reasoning".into(),
            side_effects: "side effects".into(),
            undo_instructions: "undo".into(),
        }
    }

    #[test]
    fn model_decision_is_bounded_without_truncating_action_fields() {
        let valid = serde_json::json!({
            "action": "service_restart",
            "service_name": "Spooler",
        });
        let oversized_target = "x".repeat(MAX_MODEL_ACTION_STRING_BYTES + 1);
        let mut decision = ClaudeDecision {
            analysis: format!("a{}", "é".repeat(MAX_MODEL_DECISION_TEXT_BYTES)),
            problems: std::iter::repeat_with(|| model_problem(valid.clone()))
                .take(MAX_MODEL_PROBLEMS + 1)
                .collect(),
            needs_deeper_analysis: false,
        };
        decision.problems[0].proposed_fix = serde_json::json!({
            "action": "service_restart",
            "service_name": oversized_target,
        });

        decision.bound_model_output();

        assert!(decision.analysis.len() <= MAX_MODEL_DECISION_TEXT_BYTES);
        assert!(decision.analysis.ends_with('é'));
        assert_eq!(decision.problems.len(), MAX_MODEL_PROBLEMS);
        assert!(decision
            .problems
            .iter()
            .all(|problem| { problem.proposed_fix["service_name"].as_str() == Some("Spooler") }));
    }

    fn state_with_disk_health(dh: &str) -> SystemState {
        SystemState {
            collected_at: 0,
            collector_errors: vec![],
            uptime_secs: 0,
            cpu_usage_percent: 0.0,
            memory_usage_percent: 0.0,
            memory_available_gb: 0.0,
            disk_usage_percent: 0.0,
            disk_free_gb: 0.0,
            running_services_count: 0,
            failed_services: vec![],
            network_interfaces: vec![],
            network_errors: 0,
            disk_health: dh.to_string(),
            windows_update_status: "ok".into(),
            security: SecurityPosture::default(),
        }
    }

    #[test]
    fn dedup_key_identifies_the_issue_not_the_exact_parameters() {
        // Regenerated free text must NOT defeat dedup: same issue → same key.
        assert_eq!(
            FixAction::PowerShellDiagnostic {
                script: "Get-Process | Sort CPU".into()
            }
            .dedup_key(),
            FixAction::PowerShellDiagnostic {
                script: "Get-EventLog -Newest 50".into()
            }
            .dedup_key(),
        );
        assert_eq!(
            FixAction::RegistryReset {
                key_path: "HKLM\\X".into(),
                value_name: "Start".into(),
                value_data: "2".into()
            }
            .dedup_key(),
            FixAction::RegistryReset {
                key_path: "hklm\\x".into(),
                value_name: "start".into(),
                value_data: "3".into()
            }
            .dedup_key(),
        );
        assert_eq!(
            FixAction::LogCleanup {
                path: "C:\\Logs".into(),
                days_old: 30
            }
            .dedup_key(),
            FixAction::LogCleanup {
                path: "c:\\logs".into(),
                days_old: 7
            }
            .dedup_key(),
        );
        // Windows service names are case-insensitive.
        assert_eq!(
            FixAction::ServiceRestart {
                service_name: "Spooler".into()
            }
            .dedup_key(),
            FixAction::ServiceRestart {
                service_name: "spooler".into()
            }
            .dedup_key(),
        );

        // Different issues → different keys.
        assert_ne!(
            FixAction::ServiceRestart {
                service_name: "Spooler".into()
            }
            .dedup_key(),
            FixAction::ServiceRestart {
                service_name: "W32Time".into()
            }
            .dedup_key(),
        );
        assert_ne!(
            FixAction::ServiceRestart {
                service_name: "Spooler".into()
            }
            .dedup_key(),
            FixAction::ServiceStart {
                service_name: "Spooler".into()
            }
            .dedup_key(),
        );
        // StartupSet keeps the enable flag: enable vs disable for the same entry are
        // different user intents, both allowed to queue.
        let startup = |enable: bool| FixAction::StartupSet {
            name: "OneDrive".into(),
            location: "user_run".into(),
            hive: "S-1-5-21-x".into(),
            enable,
        };
        assert_ne!(startup(true).dedup_key(), startup(false).dedup_key());
    }

    #[test]
    fn dedup_key_normalizes_equivalent_windows_targets() {
        assert_eq!(
            FixAction::LogCleanup {
                path: r"C:\Logs\.\App".into(),
                days_old: 30,
            }
            .dedup_key(),
            FixAction::LogCleanup {
                path: "c:/logs/app/".into(),
                days_old: 7,
            }
            .dedup_key(),
        );
        assert_eq!(
            FixAction::FileDelete {
                path: r"C:\Temp\Old\..\cache.bin".into(),
            }
            .dedup_key(),
            FixAction::FileDelete {
                path: "c:/temp/cache.bin".into(),
            }
            .dedup_key(),
        );
        assert_eq!(
            FixAction::RegistryReset {
                key_path: r"HKEY_LOCAL_MACHINE\Software\Eir".into(),
                value_name: "State".into(),
                value_data: "1".into(),
            }
            .dedup_key(),
            FixAction::RegistryReset {
                key_path: "hklm:/software/eir/".into(),
                value_name: "state".into(),
                value_data: "2".into(),
            }
            .dedup_key(),
        );
        assert_eq!(
            FixAction::TaskDisable {
                task_name: r"\Vendor\\Updater".into(),
            }
            .dedup_key(),
            FixAction::TaskDisable {
                task_name: r"\vendor\updater".into(),
            }
            .dedup_key(),
        );
    }

    #[test]
    fn disk_health_is_a_fault_only_when_degraded() {
        // Healthy / unknown / empty are NOT faults (no spurious analysis).
        for ok in ["Healthy", "unknown", ""] {
            assert!(state_with_disk_health(ok)
                .fault_parts()
                .iter()
                .all(|p| !p.starts_with("DISK_HEALTH")));
        }
        // Warning / Unhealthy ARE faults (case-insensitive).
        for bad in ["Warning", "Unhealthy", "UNHEALTHY"] {
            assert!(state_with_disk_health(bad)
                .fault_parts()
                .iter()
                .any(|p| p.starts_with("DISK_HEALTH")));
        }
    }
}
