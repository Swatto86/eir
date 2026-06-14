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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemState {
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExecutionResult {
    pub action: String,
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeDecision {
    pub analysis: String,
    pub problems: Vec<Problem>,
}

/// Token + cost usage for a single Claude call (claude_cli provider).
#[derive(Debug, Clone, Default)]
pub struct CallUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost_usd: f64,
}
