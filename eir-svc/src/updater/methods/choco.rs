//! The Chocolatey update method. choco runs cleanly as SYSTEM/admin, so this is a
//! first-class backend: `choco outdated` lists candidates, `choco upgrade <id> -y`
//! applies one, and we cross-check the new version via winget's ARP read (choco
//! installs register there). `choco outdated -r` emits one `name|current|available|
//! pinned` line per package — a stable machine-readable format across choco
//! versions, unlike the human `choco list` whose meaning changed between v1 and v2.

use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::methods::detect;
use crate::updater::proc::{self, INSTALL, LIST};
use crate::updater::verify::{verify_app, VerifyTarget};
use std::path::PathBuf;
use std::time::Duration;

/// One outdated Chocolatey package.
pub struct ChocoUpdate {
    pub name: String,
    pub current: String,
    pub available: String,
}

/// Parse `choco outdated -r` output: `name|currentVersion|availableVersion|pinned`.
/// Lines without the pipe layout (headers, summaries) are ignored, and pinned
/// packages are skipped (the user pinned them deliberately).
fn parse_outdated(text: &str) -> Vec<ChocoUpdate> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 || parts[0].is_empty() || parts[2].is_empty() {
            continue;
        }
        // A 4th "pinned" field of "true" means don't touch it.
        if parts
            .get(3)
            .map(|p| p.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(ChocoUpdate {
            name: parts[0].to_string(),
            current: parts[1].to_string(),
            available: parts[2].to_string(),
        });
    }
    out
}

async fn run(choco: PathBuf, args: Vec<String>, dur: Duration) -> (i32, String) {
    let mut cmd = std::process::Command::new(choco);
    cmd.args(&args);
    proc::run_capped_cmd(cmd, dur).await
}

/// List outdated Chocolatey packages.
pub async fn list_outdated() -> Vec<ChocoUpdate> {
    let Some(choco) = detect::choco_path() else {
        return Vec::new();
    };
    let (_code, out) = run(
        choco,
        vec![
            "outdated".to_string(),
            "-r".to_string(),
            "--no-color".to_string(),
        ],
        LIST,
    )
    .await;
    parse_outdated(&out)
}

/// choco upgrade exit codes that mean success (0) or success-pending-reboot.
fn choco_ok(code: i32) -> bool {
    code == 0 || code == 3010 || code == 1641
}

/// The `choco upgrade` argument list for one package. `-y` only auto-confirms
/// prompts; `-f`/`--force` is what actually re-runs an upgrade the diagnostician
/// asked to force (e.g. past a "same version" skip or a stale lock).
fn upgrade_args(pkg: &str, force: bool) -> Vec<String> {
    let mut a = vec![
        "upgrade".to_string(),
        pkg.to_string(),
        "-y".to_string(),
        "--no-progress".to_string(),
        "--no-color".to_string(),
    ];
    if force {
        a.push("--force".to_string());
    }
    a
}

/// Update one app via Chocolatey, then cross-check the version via winget's ARP read.
/// `force` honours the diagnostician's `Force` / `ClearManagerLock` remedy by adding
/// `--force`: previously the remedy was validated and accepted but never reached the
/// command, so a "force" retry re-ran the identical failed args and burned an attempt.
pub async fn attempt_with(candidate: &UpdateCandidate, force: bool) -> AttemptOutcome {
    let Some(choco) = detect::choco_path() else {
        return AttemptOutcome::failed(
            Method::Choco,
            ErrorCategory::NotFound,
            "Chocolatey is not installed",
        );
    };
    let pkg = candidate
        .package_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| candidate.name.clone());

    let (code, output) = run(choco, upgrade_args(&pkg, force), INSTALL).await;

    let mut out = AttemptOutcome::failed(Method::Choco, ErrorCategory::Unknown, String::new());
    out.exit_code = Some(code);
    if choco_ok(code) {
        // choco-installed apps register in ARP, so winget's by-name read can confirm.
        let (verification, found) = verify_app(
            &VerifyTarget::ByName {
                name: candidate.name.clone(),
                verify_exe: None,
            },
            &candidate.available,
        )
        .await;
        out.verification = verification;
        out.installed_version = (!found.is_empty()).then_some(found);
        // choco's exit code is authoritative for "it ran"; only a hard winget
        // downgrade-mismatch demotes it.
        out.success = verification != Verification::Mismatch;
        out.category = if out.success {
            None
        } else {
            Some(ErrorCategory::VerifyFailed)
        };
        out.detail = format!(
            "choco upgraded {pkg}{}",
            if code == 3010 || code == 1641 {
                " (reboot required)"
            } else {
                ""
            }
        );
    } else {
        out.category = Some(classify_error(Method::Choco, Some(code), &output));
        out.detail = clean(&output);
    }
    out
}

/// Trim choco's verbose output to the last few non-empty lines (its errors and the
/// final status live at the end), capped for display.
fn clean(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !l.starts_with("Chocolatey v") && !l.contains("validations performed")
        })
        .collect();
    let tail = lines
        .iter()
        .rev()
        .take(4)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    if tail.chars().count() > 300 {
        tail.chars().take(299).chain(['…']).collect()
    } else {
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outdated_reads_pipe_rows_and_skips_pinned() {
        // The exact shape of `choco outdated -r`, with a header line and a pinned pkg.
        let text = "Chocolatey v2.3.0\n\
                    git|2.43.0|2.45.1|false\n\
                    nodejs|20.10.0|21.6.1|false\n\
                    vlc|3.0.20|3.0.21|true\n\
                    \n\
                    Chocolatey has determined 3 package(s) are outdated.";
        let ups = parse_outdated(text);
        let names: Vec<&str> = ups.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["git", "nodejs"]); // vlc pinned -> skipped
        assert_eq!(ups[0].current, "2.43.0");
        assert_eq!(ups[0].available, "2.45.1");
    }

    #[test]
    fn upgrade_args_include_force_only_when_requested() {
        let base = upgrade_args("git", false);
        assert!(!base.iter().any(|a| a == "--force"));
        assert_eq!(base[0], "upgrade");
        assert_eq!(base[1], "git");
        assert!(upgrade_args("git", true).iter().any(|a| a == "--force"));
    }

    #[test]
    fn choco_ok_accepts_reboot_codes() {
        assert!(choco_ok(0));
        assert!(choco_ok(3010));
        assert!(choco_ok(1641));
        assert!(!choco_ok(1));
        assert!(!choco_ok(-1));
    }
}
