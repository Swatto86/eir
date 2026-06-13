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
}
