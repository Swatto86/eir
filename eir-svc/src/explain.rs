//! Deterministic, human-readable explanations of fix actions.
//!
//! These descriptions are derived purely from the action type and the target's
//! on-disk facts — never from the AI — so the user can trust them when deciding
//! whether to approve. The AI's own `side_effects`/`undo_instructions` are shown
//! alongside as supporting detail, but the figures here (a file's real size, age,
//! and kind) are ground truth.

use crate::models::FixAction;
use chrono::{DateTime, Utc};

/// A trustworthy summary of what an action does and whether it can be undone.
pub struct ActionExplanation {
    /// Plain-English description of the operation and its immediate effect.
    pub summary: String,
    /// The concrete target affected (path, service, process, …).
    pub target: String,
    /// Whether the effect can be reversed after it runs.
    pub reversible: bool,
}

/// Describe what executing `action` will do, in terms a non-expert can act on.
pub fn explain(action: &FixAction) -> ActionExplanation {
    match action {
        FixAction::ServiceRestart { service_name } => ActionExplanation {
            summary: format!(
                "Restarts the Windows service '{service_name}' (stops it, then starts it again). \
                 The service is briefly unavailable while it cycles."
            ),
            target: service_name.clone(),
            reversible: true,
        },
        FixAction::ServiceStop { service_name } => ActionExplanation {
            summary: format!(
                "Stops the Windows service '{service_name}'. It stays stopped until something \
                 starts it again."
            ),
            target: service_name.clone(),
            reversible: true,
        },
        FixAction::ServiceStart { service_name } => ActionExplanation {
            summary: format!("Starts the Windows service '{service_name}'."),
            target: service_name.clone(),
            reversible: true,
        },
        FixAction::LogCleanup { path, days_old } => ActionExplanation {
            summary: format!(
                "Deletes log files older than {days_old} days under '{path}'. Frees disk space; \
                 the removed logs cannot be recovered."
            ),
            target: path.clone(),
            reversible: false,
        },
        FixAction::DiskCleanup { target } => ActionExplanation {
            summary: format!(
                "Clears the '{target}' area (temporary files). Frees disk space; the cleared \
                 files cannot be recovered."
            ),
            target: target.clone(),
            reversible: false,
        },
        FixAction::PowerShellDiagnostic { script } => ActionExplanation {
            summary: "Runs a PowerShell script with SYSTEM privileges (full machine access). \
                      The exact script is shown below — read it before approving."
                .to_string(),
            target: "PowerShell script".to_string(),
            reversible: false,
        }
        .with_target_first_line(script),
        FixAction::TaskDisable { task_name } => ActionExplanation {
            summary: format!(
                "Disables the scheduled task '{task_name}'. It will not run again until re-enabled."
            ),
            target: task_name.clone(),
            reversible: true,
        },
        FixAction::TaskEnable { task_name } => ActionExplanation {
            summary: format!("Enables the scheduled task '{task_name}'."),
            target: task_name.clone(),
            reversible: true,
        },
        FixAction::RegistryReset {
            key_path,
            value_name,
            value_data,
        } => ActionExplanation {
            summary: format!(
                "Sets the registry value '{value_name}' under '{key_path}' to '{value_data}', \
                 overwriting whatever is there now."
            ),
            target: format!("{key_path}\\{value_name}"),
            // The prior value is snapshotted before the write (see registry::reset_value),
            // so this can be auto-undone with one click from the activity feed.
            reversible: true,
        },
        FixAction::NetworkDiagnostic { command } => explain_network(command),
        FixAction::DriverDisable { driver_name } => ActionExplanation {
            summary: format!(
                "Disables the driver '{driver_name}'. The hardware or feature it provides stops \
                 working until the driver is re-enabled."
            ),
            target: driver_name.clone(),
            reversible: true,
        },
        FixAction::DriverEnable { driver_name } => ActionExplanation {
            summary: format!("Enables the driver '{driver_name}'."),
            target: driver_name.clone(),
            reversible: true,
        },
        FixAction::SoftwareUninstall { package_name } => ActionExplanation {
            summary: format!(
                "Uninstalls '{package_name}'. (Blocked by policy — Eir never removes software, \
                 so this will not run even if approved.)"
            ),
            target: package_name.clone(),
            reversible: false,
        },
        FixAction::BcdEdit { element, value } => ActionExplanation {
            summary: format!(
                "Changes the boot configuration setting '{element}' to '{value}'. This affects \
                 how Windows starts up."
            ),
            target: element.clone(),
            // The prior BCD value is not snapshotted, so this cannot be auto-undone
            // (unlike RegistryReset, which now captures its prior value).
            reversible: false,
        },
        FixAction::ProcessKill { process_name } => ActionExplanation {
            summary: format!(
                "Force-closes every running process named '{process_name}'. Any unsaved work in \
                 those processes is lost."
            ),
            target: process_name.clone(),
            reversible: false,
        },
        FixAction::FileDelete { path } => ActionExplanation {
            summary: format!(
                "Permanently deletes the file '{path}'. It is removed with force and does NOT go \
                 to the Recycle Bin, so it cannot be restored from there."
            ),
            target: path.clone(),
            reversible: false,
        },
        FixAction::FirewallEnable { profile } => ActionExplanation {
            summary: format!(
                "Turns the Windows Firewall back on for the '{profile}' profile. This restores a \
                 protective default and can be turned off again."
            ),
            target: profile.clone(),
            reversible: true,
        },
        FixAction::DefenderSignatureUpdate => ActionExplanation {
            summary: "Updates Windows Defender's virus definitions to the latest available. \
                      Only refreshes definitions — it changes no settings."
                .to_string(),
            target: "Windows Defender".to_string(),
            reversible: true,
        },
        FixAction::DefenderRealtimeEnable => ActionExplanation {
            summary:
                "Turns Windows Defender real-time (on-access) protection back on. Reversible, \
                      but only approve this if you did not deliberately disable it (e.g. for a \
                      different antivirus)."
                    .to_string(),
            target: "Windows Defender real-time protection".to_string(),
            reversible: true,
        },
        FixAction::SfcScan => ActionExplanation {
            summary: "Runs System File Checker (sfc /scannow) to verify and repair protected \
                      Windows system files. It only repairs corruption from Windows' own cache; \
                      it takes several minutes and cannot be interrupted cleanly."
                .to_string(),
            target: "Protected system files".to_string(),
            reversible: false,
        },
        FixAction::DismRestoreHealth => ActionExplanation {
            summary: "Runs DISM /Online /Cleanup-Image /RestoreHealth to repair the Windows \
                      component store (the source SFC repairs from). It can take many minutes and \
                      may download replacement files from Windows Update."
                .to_string(),
            target: "Windows component store".to_string(),
            reversible: false,
        },
    }
}

impl ActionExplanation {
    /// Append the first line of a long target (used for the PowerShell script) so
    /// the queue list shows a hint; the full text goes in `target_details`.
    fn with_target_first_line(mut self, full: &str) -> Self {
        if let Some(first) = full.lines().find(|l| !l.trim().is_empty()) {
            let trimmed = first.trim();
            let snippet: String = trimmed.chars().take(60).collect();
            let ellipsis = if trimmed.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            self.target = format!("PowerShell: {snippet}{ellipsis}");
        }
        self
    }
}

fn explain_network(command: &str) -> ActionExplanation {
    let (summary, reversible) = match command.to_lowercase().as_str() {
        "flush_dns" => (
            "Flushes the DNS resolver cache. Cached name lookups are cleared and rebuilt on \
             demand — safe and self-healing.",
            true,
        ),
        "release_renew" => (
            "Releases and renews the network adapter's DHCP lease. Connectivity drops for a \
             moment while a new address is obtained.",
            true,
        ),
        "reset_tcp" => (
            "Resets the TCP/IP stack. Requires a reboot to fully take effect and resets network \
             tuning to defaults.",
            false,
        ),
        "reset_winsock" => (
            "Resets the Winsock catalog. Requires a reboot; third-party network add-ons (some \
             VPN/firewall layers) may need reinstalling afterwards.",
            false,
        ),
        other => {
            return ActionExplanation {
                summary: format!("Runs the network diagnostic '{other}'."),
                target: other.to_string(),
                reversible: false,
            }
        }
    };
    ActionExplanation {
        summary: summary.to_string(),
        target: command.to_string(),
        reversible,
    }
}

/// Gather human-readable, factual detail about the action's target by inspecting
/// the system — currently the on-disk facts for `file_delete` and the full script
/// for `powershell_diagnostic`. Returns an empty string when there is nothing
/// inspectable. Does file I/O; call off the hot path.
pub fn target_details(action: &FixAction) -> String {
    match action {
        FixAction::FileDelete { path } => file_facts(path),
        FixAction::PowerShellDiagnostic { script } => format!("Script to run:\n{script}"),
        _ => String::new(),
    }
}

/// Describe a file the way a cautious admin would check it before deleting:
/// does it exist, how big is it, when was it last written, and what kind of file
/// does it look like (regenerable cache vs. irreplaceable user data).
fn file_facts(path: &str) -> String {
    let p = std::path::Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => {
            return "This file does not currently exist (it may already be gone, or the path is \
                    wrong). Nothing would be deleted."
                .to_string();
        }
    };

    if meta.is_dir() {
        return "This path is a DIRECTORY, not a file. Eir refuses to delete directories, so this \
                action would be rejected at execution."
            .to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("Size: {}", human_size(meta.len())));

    if let Ok(modified) = meta.modified() {
        let dt: DateTime<Utc> = modified.into();
        let age = Utc::now()
            .signed_duration_since(dt)
            .to_std()
            .map(human_age)
            .unwrap_or_default();
        lines.push(format!(
            "Last modified: {} UTC{}",
            dt.format("%Y-%m-%d %H:%M"),
            if age.is_empty() {
                String::new()
            } else {
                format!(" ({age} ago)")
            }
        ));
    }

    if meta.permissions().readonly() {
        lines.push("Marked read-only.".to_string());
    }

    lines.push(classify_file(path));
    lines.join("\n")
}

/// A hedged guess at what kind of file this is, to flag the difference between a
/// throwaway cache and someone's only copy of a document.
fn classify_file(path: &str) -> String {
    let lower = path.to_lowercase();
    let dir_hits = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let ext_is = |exts: &[&str]| {
        std::path::Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.contains(&e))
            .unwrap_or(false)
    };

    if dir_hits(&[
        "\\cache",
        "\\temp",
        "\\tmp\\",
        "\\prefetch",
        "crashdump",
        "minidump",
    ]) || ext_is(&["tmp", "dmp", "etl", "old", "bak", "lock"])
    {
        "Looks like a regenerable cache / temp / crash file — programs normally recreate these \
         automatically, so deleting it is usually low-risk."
            .to_string()
    } else if dir_hits(&[
        "\\documents",
        "\\desktop",
        "\\pictures",
        "\\downloads",
        "\\videos",
        "\\music",
        "\\onedrive",
    ]) {
        "Looks like it lives in a personal folder (Documents/Desktop/etc.) — deleting it could be \
         permanent loss of your own data. Be sure before approving."
            .to_string()
    } else if ext_is(&[
        "json", "xml", "ini", "cfg", "conf", "config", "db", "sqlite", "dat",
    ]) {
        "Looks like a configuration or data file — the program may lose saved state or settings. \
         Many apps rebuild defaults on next launch, but confirm this one does."
            .to_string()
    } else {
        "Could not classify this file from its path — review it manually before approving."
            .to_string()
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn human_age(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        "less than a minute".to_string()
    } else if secs < 3600 {
        format!("{} minute(s)", secs / 60)
    } else if secs < 86400 {
        format!("{} hour(s)", secs / 3600)
    } else {
        format!("{} day(s)", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_delete_is_irreversible_and_targets_the_path() {
        let action = FixAction::FileDelete {
            path: "C:\\x\\y.tmp".to_string(),
        };
        let e = explain(&action);
        assert!(!e.reversible);
        assert_eq!(e.target, "C:\\x\\y.tmp");
        assert!(e.summary.to_lowercase().contains("delete"));
    }

    #[test]
    fn missing_file_says_nothing_to_delete() {
        let details = file_facts("C:\\definitely\\not\\here\\nope.bin");
        assert!(details.contains("does not currently exist"));
    }

    #[test]
    fn cache_path_is_flagged_low_risk() {
        assert!(
            classify_file("C:\\Users\\a\\AppData\\Local\\App\\Cache\\x.db")
                .to_lowercase()
                .contains("low-risk")
        );
    }

    #[test]
    fn documents_path_is_flagged_risky() {
        assert!(classify_file("C:\\Users\\a\\Documents\\thesis.json")
            .to_lowercase()
            .contains("permanent loss"));
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
    }
}
