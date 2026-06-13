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
    ServiceRestart { service_name: String },
    ServiceStop { service_name: String },
    ServiceStart { service_name: String },
    LogCleanup { path: String, days_old: u32 },
    DiskCleanup { target: String },
    PowerShellDiagnostic { script: String },
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
