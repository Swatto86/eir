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
    /// Action types that are never executed — not even with approval.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Scheduled-task path prefixes that task_disable/task_enable may never target.
    /// `#[serde(default)]` so an older `policy.toml` without the key still loads.
    #[serde(default)]
    pub tasks: Vec<String>,
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
        let name = action_type_name(action);

        // Action-type blocklist — never executed, not even with approval.
        if self.blocklist.actions.iter().any(|a| a == name) {
            return Verdict::Block(format!("Action '{name}' is disabled by policy"));
        }

        // Hard target blocklist (specific services / paths)
        if let Some(reason) = self.blocked_reason(action) {
            return Verdict::Block(reason);
        }

        // Below the configured confidence threshold — don't run or prompt.
        let threshold = self.execution.confidence_threshold;
        if confidence < threshold {
            return Verdict::Block(format!(
                "Confidence {:.0}% below threshold {:.0}%",
                confidence * 100.0,
                threshold * 100.0,
            ));
        }

        // Not on the auto-execute whitelist — the catastrophic actions
        // (bcd_edit, driver_disable, powershell_diagnostic) still require
        // explicit approval regardless of confidence.
        if !self.whitelist.actions.iter().any(|a| a == name) {
            return Verdict::RequireApproval(format!(
                "Action '{name}' needs approval (not auto-executed)"
            ));
        }

        Verdict::AutoApprove
    }

    fn blocked_reason(&self, action: &FixAction) -> Option<String> {
        match action {
            // A UNC / network path in a filesystem action makes the LocalSystem service
            // authenticate to a remote host (SMB), which is a credential-relay vector when
            // the path is AI-influenced — and for FileDelete the approval-card preview
            // (explain::file_facts) would touch it before a human even approves. Refuse
            // before either the executor OR the approval card runs. (Extends the updater's
            // C2 drive-letter-only guard to the executor path actions.)
            FixAction::LogCleanup { path, .. } | FixAction::FileDelete { path }
                if is_network_path(path) =>
            {
                Some(format!(
                    "Refusing '{path}' — network/UNC paths are not allowed"
                ))
            }
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
            FixAction::TaskDisable { task_name } | FixAction::TaskEnable { task_name }
                if self.task_blocked(task_name) =>
            {
                Some(format!("Scheduled task '{task_name}' is on the blocklist"))
            }
            _ => None,
        }
    }

    fn service_blocked(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.blocklist
            .services
            .iter()
            .any(|b| b.to_lowercase() == lower)
    }

    fn path_blocked(&self, path: &str) -> bool {
        self.blocklist.paths.iter().any(|b| is_within(path, b))
    }

    /// True if `task_name` equals or sits under any blocklisted task-path prefix,
    /// compared on normalised components so case/separator tricks can't evade it.
    /// Only matches fully-qualified names (`\Folder\Name`); the executor rejects all
    /// bare names before resolution so a leaf cannot hide a blocked Microsoft path.
    fn task_blocked(&self, task_name: &str) -> bool {
        self.blocklist.tasks.iter().any(|b| is_within(task_name, b))
    }
}

/// True if `path` equals or is a descendant of `ancestor`, compared on normalised
/// path/registry components — so separators, case, `\\?\`, `.`/`..`, and hive
/// aliases can't evade it. An empty `ancestor` never matches. Shared by the policy
/// blocklist and the registry executor's allow/deny lists so both agree on what
/// "under this key" means (`C:\Windows` matches `C:\Windows\...` but not
/// `C:\WindowsApps`).
/// True for a UNC / network path (`\\server\share`, `//server/share`, `\\?\UNC\...`).
/// Such a path makes the LocalSystem service authenticate to a remote host, which is a
/// credential-relay vector when the path is AI-influenced — never a valid local fix
/// target. The extended-local form `\\?\C:\...` also matches (it starts with `\\`);
/// that's refused too, deliberately, since the plain `C:\` form is always available.
pub(crate) fn is_network_path(path: &str) -> bool {
    path.trim_start().replace('/', "\\").starts_with("\\\\")
}

pub(crate) fn is_within(path: &str, ancestor: &str) -> bool {
    let cand = normalize_path_lexical(path);
    let anc = normalize_path_lexical(ancestor);
    !anc.is_empty() && cand.len() >= anc.len() && cand[..anc.len()] == anc[..]
}

/// Lexically normalise a path (or registry key) into lowercased components for a
/// robust blocklist comparison, WITHOUT touching the filesystem (the target may not
/// exist yet). Folds registry hive aliases (`HKEY_LOCAL_MACHINE` → `HKLM:`) so the
/// gate and the executor agree, strips a `\\?\` prefix, accepts both `\` and `/`
/// separators, and resolves `.`/`..` — all the forms a raw `starts_with` missed,
/// letting `C:/Windows/...`, `\\?\C:\Windows\...`, or `C:\Windows\..\Windows\...`
/// slip past a `C:\Windows` block.
pub(crate) fn normalize_path_lexical(path: &str) -> Vec<String> {
    let mut p = path.trim().to_string();
    for (from, to) in [
        ("HKEY_LOCAL_MACHINE", "HKLM:"),
        ("HKEY_CURRENT_USER", "HKCU:"),
    ] {
        if p.len() >= from.len() && p[..from.len()].eq_ignore_ascii_case(from) {
            p = format!("{to}{}", &p[from.len()..]);
            break;
        }
    }
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    let mut comps: Vec<String> = Vec::new();
    for raw in p.split(['\\', '/']) {
        match raw {
            "" | "." => {}
            ".." => {
                // Never pop past the drive/root component — Windows treats `..` at the
                // root as a no-op, so `C:\Windows\..\..\Windows\System32` resolves back
                // to `C:\Windows\System32`. Popping the drive would let such a path
                // escape a `C:\Windows` block.
                if comps.len() > 1 {
                    comps.pop();
                }
            }
            c => comps.push(c.to_lowercase()),
        }
    }
    comps
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
        FixAction::FirewallEnable { .. } => "firewall_enable",
        FixAction::DefenderSignatureUpdate => "defender_signature_update",
        FixAction::DefenderRealtimeEnable => "defender_realtime_enable",
        FixAction::SfcScan => "sfc_scan",
        FixAction::DismRestoreHealth => "dism_restore_health",
        FixAction::StartupSet { .. } => "startup_set",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blocklisted action must be blocked even at max confidence and even if
    /// it also appears on the whitelist — there is no path to execute it.
    #[test]
    fn blocklisted_action_is_always_blocked() {
        let pol = ExecutionPolicy {
            execution: ExecutionConfig {
                confidence_threshold: 0.85,
                max_retries_per_issue: 3,
                rate_limit_mins: 30,
                auto_approve_on_success_rate: 0.95,
            },
            whitelist: WhitelistConfig {
                actions: vec!["software_uninstall".into()],
            },
            blocklist: BlocklistConfig {
                services: vec![],
                paths: vec![],
                actions: vec!["software_uninstall".into()],
                tasks: vec![],
            },
        };
        let action = FixAction::SoftwareUninstall {
            package_name: "NordVPN".into(),
        };
        assert!(matches!(pol.evaluate(&action, 0.99), Verdict::Block(_)));
    }

    fn policy_with(whitelist: &[&str]) -> ExecutionPolicy {
        ExecutionPolicy {
            execution: ExecutionConfig {
                confidence_threshold: 0.80,
                max_retries_per_issue: 3,
                rate_limit_mins: 30,
                auto_approve_on_success_rate: 0.95,
            },
            whitelist: WhitelistConfig {
                actions: whitelist.iter().map(|s| s.to_string()).collect(),
            },
            blocklist: BlocklistConfig {
                services: vec![],
                paths: vec![],
                actions: vec![],
                tasks: vec![],
            },
        }
    }

    /// A whitelisted security fix (firewall_enable) auto-runs at/above threshold,
    /// while a security-sensitive one left off the whitelist still needs approval.
    #[test]
    fn security_actions_follow_the_whitelist() {
        let pol = policy_with(&["firewall_enable", "defender_signature_update"]);

        let fw = FixAction::FirewallEnable {
            profile: "all".into(),
        };
        assert!(matches!(pol.evaluate(&fw, 0.90), Verdict::AutoApprove));
        // Below the confidence threshold it is blocked, not auto-run.
        assert!(matches!(pol.evaluate(&fw, 0.50), Verdict::Block(_)));

        // Not whitelisted → approval required even at high confidence.
        assert!(matches!(
            pol.evaluate(&FixAction::DefenderRealtimeEnable, 0.99),
            Verdict::RequireApproval(_)
        ));
    }

    /// SFC/DISM are long-running system-file repairs and must NEVER auto-execute, even
    /// at max confidence — they are approval-gated by being off the whitelist.
    #[test]
    fn system_file_repairs_always_require_approval() {
        let pol = policy_with(&["service_restart", "firewall_enable"]);
        assert!(matches!(
            pol.evaluate(&FixAction::SfcScan, 0.99),
            Verdict::RequireApproval(_)
        ));
        assert!(matches!(
            pol.evaluate(&FixAction::DismRestoreHealth, 0.99),
            Verdict::RequireApproval(_)
        ));
    }

    fn policy_blocking_path(path: &str) -> ExecutionPolicy {
        ExecutionPolicy {
            execution: ExecutionConfig {
                confidence_threshold: 0.80,
                max_retries_per_issue: 3,
                rate_limit_mins: 30,
                auto_approve_on_success_rate: 0.95,
            },
            whitelist: WhitelistConfig {
                actions: vec!["file_delete".into()],
            },
            blocklist: BlocklistConfig {
                services: vec![],
                paths: vec![path.into()],
                actions: vec![],
                tasks: vec![],
            },
        }
    }

    #[test]
    fn path_blocklist_resists_separator_and_traversal_bypasses() {
        let pol = policy_blocking_path("C:\\Windows");
        let blocked = |p: &str| {
            matches!(
                pol.evaluate(&FixAction::FileDelete { path: p.into() }, 0.99),
                Verdict::Block(_)
            )
        };
        // Straight match and all the evasions that beat a raw `starts_with`.
        assert!(blocked("C:\\Windows\\System32\\x.dll"));
        assert!(blocked("c:\\windows\\system32\\x.dll")); // case
        assert!(blocked("C:/Windows/System32/x.dll")); // forward slashes
        assert!(blocked("\\\\?\\C:\\Windows\\System32\\x.dll")); // \\?\ prefix
        assert!(blocked("C:\\Windows\\..\\Windows\\System32\\x.dll")); // traversal
                                                                       // Traversal that tries to escape past the drive root still resolves back into
                                                                       // C:\Windows on Windows, so it must stay blocked (the drive is never popped).
        assert!(blocked("C:\\Windows\\..\\..\\Windows\\System32\\x.dll"));
        // A sibling directory that only shares a name prefix is NOT blocked.
        assert!(!blocked("C:\\WindowsApps\\ok.txt"));
    }

    #[test]
    fn unc_paths_are_blocked_for_filesystem_actions() {
        // A UNC/network path in LogCleanup (auto-whitelisted) or FileDelete must be blocked
        // by policy — before the executor runs OR the FileDelete approval preview stats it.
        let pol = policy_with(&["log_cleanup", "file_delete"]);
        for p in [
            r"\\attacker\share\logs",
            "//attacker/share/logs",
            r"\\?\UNC\attacker\share\logs",
        ] {
            assert!(
                matches!(
                    pol.evaluate(
                        &FixAction::LogCleanup {
                            path: p.into(),
                            days_old: 7
                        },
                        0.99
                    ),
                    Verdict::Block(_)
                ),
                "LogCleanup UNC not blocked: {p}"
            );
            assert!(
                matches!(
                    pol.evaluate(&FixAction::FileDelete { path: p.into() }, 0.99),
                    Verdict::Block(_)
                ),
                "FileDelete UNC not blocked: {p}"
            );
        }
        // A normal local path is unaffected (LogCleanup auto-approves; FileDelete needs
        // approval since it's whitelisted here but is a per-file action).
        assert!(matches!(
            pol.evaluate(
                &FixAction::LogCleanup {
                    path: r"C:\Logs\App".into(),
                    days_old: 7
                },
                0.99
            ),
            Verdict::AutoApprove
        ));
        assert!(is_network_path(r"\\a\b"));
        assert!(is_network_path("//a/b"));
        assert!(!is_network_path(r"C:\Logs\App"));
        assert!(!is_network_path(r"D:\x"));
    }

    #[test]
    fn blocklisted_scheduled_task_is_never_auto_executed() {
        // task_disable/task_enable are auto-whitelisted, so a fully-qualified critical
        // task must be blocked by the target blocklist even at max confidence — and the
        // block must survive case- and separator-variant names.
        let pol = ExecutionPolicy {
            execution: ExecutionConfig {
                confidence_threshold: 0.80,
                max_retries_per_issue: 3,
                rate_limit_mins: 30,
                auto_approve_on_success_rate: 0.95,
            },
            whitelist: WhitelistConfig {
                actions: vec!["task_disable".into(), "task_enable".into()],
            },
            blocklist: BlocklistConfig {
                services: vec![],
                paths: vec![],
                actions: vec![],
                tasks: vec!["\\Microsoft\\Windows\\Windows Defender".into()],
            },
        };
        let blocked = |name: &str| {
            matches!(
                pol.evaluate(
                    &FixAction::TaskDisable {
                        task_name: name.into()
                    },
                    0.99
                ),
                Verdict::Block(_)
            )
        };
        assert!(blocked(
            "\\Microsoft\\Windows\\Windows Defender\\Windows Defender Scheduled Scan"
        ));
        assert!(blocked(
            "/microsoft/windows/windows defender/Windows Defender Cache Maintenance"
        )); // case + forward slashes
        assert!(matches!(
            pol.evaluate(
                &FixAction::TaskEnable {
                    task_name: "\\Microsoft\\Windows\\Windows Defender\\Scan".into()
                },
                0.99
            ),
            Verdict::Block(_)
        ));
        // A benign third-party task is unaffected and still auto-approves.
        assert!(matches!(
            pol.evaluate(
                &FixAction::TaskDisable {
                    task_name: "\\VendorApp\\Updater".into()
                },
                0.99
            ),
            Verdict::AutoApprove
        ));
    }

    #[test]
    fn registry_hive_alias_is_blocked_like_the_drive_form() {
        // A blocklist written in HKLM: form must also catch the HKEY_* form the AI
        // might emit (and vice-versa) — they normalise to the same components.
        let pol = policy_blocking_path("HKLM:\\SYSTEM\\CurrentControlSet\\Services\\Critical");
        let action = FixAction::RegistryReset {
            key_path: "HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\Critical".into(),
            value_name: "Start".into(),
            value_data: "4".into(),
        };
        assert!(matches!(pol.evaluate(&action, 0.99), Verdict::Block(_)));
    }
}
