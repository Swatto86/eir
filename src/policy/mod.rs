use crate::models::FixAction;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct ExecutionPolicy {
    pub execution: ExecutionConfig,
    pub whitelist: WhitelistConfig,
    pub blocklist: BlocklistConfig,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // max_retries_per_issue and auto_approve_on_success_rate used in Phase 4
pub struct ExecutionConfig {
    pub confidence_threshold: f32,
    pub max_retries_per_issue: usize,
    pub rate_limit_mins: u32,
    pub auto_approve_on_success_rate: f32,
}

#[derive(Debug, Deserialize)]
pub struct WhitelistConfig {
    pub actions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct BlocklistConfig {
    pub services: Vec<String>,
    pub paths: Vec<String>,
}

pub enum Verdict {
    AutoApprove,
    RequireApproval(String),
    Block(String),
}

impl ExecutionPolicy {
    pub fn load(path: &str) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Failed to read policy file: {path}"))?;
        toml::from_str(&text).with_context(|| "Failed to parse policy TOML")
    }

    pub fn evaluate(&self, action: &FixAction, confidence: f32) -> Verdict {
        // Hard blocklist first
        if let Some(reason) = self.blocked_reason(action) {
            return Verdict::Block(reason);
        }

        // Absolute floor — don't even prompt below 80%
        if confidence < 0.80 {
            return Verdict::Block(format!(
                "Confidence {:.0}% below minimum 80%",
                confidence * 100.0
            ));
        }

        // Whitelist check
        let name = action_type_name(action);
        if !self.whitelist.actions.iter().any(|a| a == name) {
            return Verdict::RequireApproval(format!(
                "Action '{name}' not on auto-execute whitelist"
            ));
        }

        // Auto-execute threshold
        if confidence >= self.execution.confidence_threshold {
            Verdict::AutoApprove
        } else {
            Verdict::RequireApproval(format!(
                "Confidence {:.0}% below auto-execute threshold {:.0}%",
                confidence * 100.0,
                self.execution.confidence_threshold * 100.0,
            ))
        }
    }

    fn blocked_reason(&self, action: &FixAction) -> Option<String> {
        match action {
            FixAction::ServiceRestart { service_name }
            | FixAction::ServiceStop { service_name }
            | FixAction::ServiceStart { service_name }
                if self.service_blocked(service_name) =>
            {
                Some(format!("Service '{service_name}' is on the blocklist"))
            }
            FixAction::LogCleanup { path, .. } if self.path_blocked(path) => {
                Some(format!("Path '{path}' is on the blocklist"))
            }
            FixAction::RegistryReset { key_path, .. } if self.path_blocked(key_path) => {
                Some(format!("Registry path '{key_path}' is on the blocklist"))
            }
            FixAction::FileDelete { path } if self.path_blocked(path) => {
                Some(format!("Path '{path}' is on the blocklist"))
            }
            _ => None,
        }
    }

    fn service_blocked(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.blocklist.services.iter().any(|b| b.to_lowercase() == lower)
    }

    fn path_blocked(&self, path: &str) -> bool {
        let lower = path.to_lowercase();
        self.blocklist
            .paths
            .iter()
            .any(|b| lower.starts_with(&b.to_lowercase()))
    }
}

fn action_type_name(action: &FixAction) -> &'static str {
    match action {
        FixAction::ServiceRestart { .. } => "service_restart",
        FixAction::ServiceStop { .. } => "service_stop",
        FixAction::ServiceStart { .. } => "service_start",
        FixAction::LogCleanup { .. } => "log_cleanup",
        FixAction::DiskCleanup { .. } => "disk_cleanup",
        FixAction::PowerShellDiagnostic { .. } => "powershell_diagnostic",
        FixAction::TaskDisable { .. } => "task_disable",
        FixAction::TaskEnable { .. } => "task_enable",
        FixAction::RegistryReset { .. } => "registry_reset",
        FixAction::NetworkDiagnostic { .. } => "network_diagnostic",
        FixAction::DriverDisable { .. } => "driver_disable",
        FixAction::DriverEnable { .. } => "driver_enable",
        FixAction::SoftwareUninstall { .. } => "software_uninstall",
        FixAction::BcdEdit { .. } => "bcd_edit",
        FixAction::ProcessKill { .. } => "process_kill",
        FixAction::FileDelete { .. } => "file_delete",
    }
}
