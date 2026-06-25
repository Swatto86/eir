//! The winget update method. Runs directly as SYSTEM (no `Start-Process -Verb
//! RunAs`, no UAC — the service is already elevated), captures winget's own output
//! so a failure carries its real reason, retries once with `--force` only for the
//! self-updating-portable case, and verifies the version actually moved. The output
//! cleaning was ported verbatim with its tests.

use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::verify::{verify_app, VerifyTarget};
use crate::updater::winget_parse::{parse_upgrades, AppUpdate};
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW — keep the console-based winget hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// List apps with an available update (`winget upgrade`). Runs unprivileged-style
/// (listing needs no special rights) and parses the fixed-width table.
pub async fn list_updates() -> Vec<AppUpdate> {
    let (_code, out) = run_winget(vec![
        "upgrade".to_string(),
        "--include-unknown".to_string(),
        "--accept-source-agreements".to_string(),
        "--disable-interactivity".to_string(),
    ])
    .await;
    parse_upgrades(&out)
}

/// The full `winget upgrade` argument list for one id, optionally forcing past the
/// portable-integrity check.
fn upgrade_args(id: &str, force: bool) -> Vec<String> {
    let mut a = vec![
        "upgrade".to_string(),
        "--id".to_string(),
        id.to_string(),
        "--exact".to_string(),
        "--silent".to_string(),
        "--accept-package-agreements".to_string(),
        "--accept-source-agreements".to_string(),
        "--disable-interactivity".to_string(),
    ];
    if force {
        a.push("--force".to_string());
    }
    a
}

/// Run winget directly and capture its merged output. The service is SYSTEM, so no
/// elevation wrapper is needed; winget also suppresses its live progress bar when
/// stdout isn't a console, which keeps the capture clean. Shared with the Store
/// method (which is winget with `--source msstore`).
pub(crate) async fn run_winget(args: Vec<String>) -> (i32, String) {
    let res = tokio::task::spawn_blocking(move || {
        std::process::Command::new("winget")
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await;
    match res {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr);
            if !e.trim().is_empty() {
                s.push('\n');
                s.push_str(e.trim());
            }
            (o.status.code().unwrap_or(-1), s)
        }
        Ok(Err(e)) => (-1, format!("winget could not be launched: {e}")),
        Err(e) => (-1, format!("winget task failed: {e}")),
    }
}

/// winget refused because a portable package's files changed after install — its
/// documented remedy is to re-run with --force. Self-updating CLIs (e.g. the GitHub
/// Copilot CLI) trip this on every upgrade.
fn portable_modified(output: &str) -> bool {
    let l = output.to_lowercase();
    l.contains("has been modified") && l.contains("--force")
}

/// Attempt to update one app via winget, then verify the version moved. Auto-retries
/// once with --force for the portable-modified case.
pub async fn attempt(candidate: &UpdateCandidate) -> AttemptOutcome {
    attempt_with(candidate, false).await
}

/// As [`attempt`], but `force_first` runs the very first upgrade with `--force` —
/// used when the AI diagnostician requests the Force remedy.
pub async fn attempt_with(candidate: &UpdateCandidate, force_first: bool) -> AttemptOutcome {
    let id = match candidate.package_id.as_deref() {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            return AttemptOutcome::failed(
                Method::Winget,
                ErrorCategory::NotFound,
                "no winget package id for this app",
            )
        }
    };

    let (mut code, mut output) = run_winget(upgrade_args(&id, force_first)).await;
    // Only auto-escalate to --force when we didn't already start with it.
    if !force_first && code != 0 && portable_modified(&output) {
        let (c, o) = run_winget(upgrade_args(&id, true)).await;
        code = c;
        output = o;
    }
    let clean = clean_winget_output(&output);

    let mut out = AttemptOutcome::failed(Method::Winget, ErrorCategory::Unknown, String::new());
    out.exit_code = Some(code);
    if code == 0 {
        let (verification, found) = verify_app(
            &VerifyTarget::Winget { id: id.clone() },
            &candidate.available,
        )
        .await;
        out.verification = verification;
        out.installed_version = (!found.is_empty()).then_some(found);
        out.success = verification != Verification::Mismatch;
        out.category = if out.success {
            None
        } else {
            Some(ErrorCategory::VerifyFailed)
        };
        out.detail = if !clean.is_empty() {
            clean
        } else if out.success {
            "updated".to_string()
        } else {
            "winget reported success but the version did not move".to_string()
        };
    } else {
        out.category = Some(classify_error(Method::Winget, Some(code), &output));
        out.detail = if clean.is_empty() {
            format!("winget exited with code {code}")
        } else {
            clean
        };
    }
    out
}

// ── winget output cleaning (ported verbatim with its tests) ───────────────────

/// Every char in `tok` is part of winget's download bar: an ASCII spinner frame
/// (`-\|/`) or a block/shade glyph. The trailing set is the CP850/437 mojibake the
/// E2 96 xx block glyphs decode to on a console that ignores the UTF-8 override.
fn is_bar_token(tok: &str) -> bool {
    !tok.is_empty()
        && tok.chars().all(|c| {
            matches!(
                c,
                '-' | '\\'
                    | '|'
                    | '/'
                    | '░'
                    | '▒'
                    | '▓'
                    | '█'
                    | '▏'
                    | '▎'
                    | '▍'
                    | '▌'
                    | '▋'
                    | '▊'
                    | '▉'
                    | '■'
                    | '□'
                    | '▬'
                    | 'Ô'
                    | 'û'
                    | 'Æ'
                    | 'ê'
                    | 'æ'
                    | 'ô'
            )
        })
}

/// A token from winget's "<n> KB / <n> MB" download counter: a number or a unit.
fn is_byte_count_token(tok: &str) -> bool {
    matches!(
        tok.to_ascii_uppercase().as_str(),
        "KB" | "MB" | "GB" | "TB" | "B" | "%"
    ) || tok
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
}

/// A whole line that is nothing but winget's live progress UI — spinner frames,
/// the download bar, and its byte counter — and so carries no message to show.
fn is_progress_noise(line: &str) -> bool {
    let mut saw_token = false;
    for tok in line.split_whitespace() {
        saw_token = true;
        if !(is_bar_token(tok) || is_byte_count_token(tok)) {
            return false;
        }
    }
    saw_token
}

/// winget's verbose step-by-step chatter and licence boilerplate — narration the
/// row's badge and version already convey. Dropped from the summary; genuine status
/// ("Successfully installed") and error lines are never matched here.
fn is_winget_chatter(line: &str) -> bool {
    let low = line.to_lowercase();
    line.starts_with("Found ")
        || low.starts_with("downloading ")
        || low.starts_with("starting package install")
        || low.starts_with("successfully verified")
        || low.contains("licensed to you by its owner")
        || low.contains("microsoft is not responsible")
}

/// Distil winget's raw capture into a concise, readable message. winget repaints its
/// spinner and download bar in place with carriage returns, so the capture is a
/// run-on of UI frames; split those back out, drop the progress noise, licence
/// boilerplate, and step chatter, then join what's left. Real status and error lines
/// always survive, so a failure still carries winget's own reason.
pub(crate) fn clean_winget_output(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for seg in raw.split(['\r', '\n']) {
        let seg = seg.trim();
        if seg.is_empty() || is_progress_noise(seg) || is_winget_chatter(seg) {
            continue;
        }
        let collapsed = seg.split_whitespace().collect::<Vec<_>>().join(" ");
        if lines.last().map(String::as_str) != Some(collapsed.as_str()) {
            lines.push(collapsed);
        }
    }
    let joined = lines.join(" · ");
    if joined.chars().count() > 300 {
        joined.chars().take(299).chain(['…']).collect()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_modified_detects_the_force_guard() {
        let out = "Starting package install...\n\
                   Unable to remove Portable package as it has been modified; \
                   to override this check use --force";
        assert!(portable_modified(out));
        assert!(!portable_modified("Installer failed with exit code: 1603"));
        assert!(!portable_modified("No applicable upgrade found."));
    }

    #[test]
    fn upgrade_args_appends_force_only_when_asked() {
        let plain = upgrade_args("GitHub.Copilot", false);
        assert_eq!(plain.first().map(String::as_str), Some("upgrade"));
        assert!(plain.iter().any(|a| a == "GitHub.Copilot"));
        assert!(!plain.iter().any(|a| a == "--force"));
        assert!(upgrade_args("GitHub.Copilot", true)
            .iter()
            .any(|a| a == "--force"));
    }

    #[test]
    fn clean_winget_output_strips_progress_noise() {
        let raw = "Found GitHub CLI [GitHub.cli] Version 2.95.0\n\
                   This application is licensed to you by its owner.\n\
                   Microsoft is not responsible for, nor does it grant any licenses to, third-party packages.\n\
                   Downloading https://github.com/cli/cli/releases/download/v2.95.0/gh_2.95.0_windows_amd64.msi\n\
                   \r  - \r  \\ \r  | \r  / \
                   \r  ██████████  1024 KB / 14.3 MB\
                   \r  ████████████████████████████  14.3 MB / 14.3 MB\n\
                   Successfully verified installer hash\n\
                   Starting package install...\n\
                   \r  - \r  \\ \r\
                   Successfully installed";
        assert_eq!(clean_winget_output(raw), "Successfully installed");
    }

    #[test]
    fn clean_winget_output_strips_oem_mojibake_bar() {
        let raw = "Downloading https://example.com/app.msi\r\
                   ÔûÆÔûÆÔûÆÔûÆ  512 KB / 9.0 MB\r\
                   ÔûÆÔûÆÔûÆÔûÆÔûÆÔûÆ  9.0 MB / 9.0 MB\n\
                   Successfully installed";
        assert_eq!(clean_winget_output(raw), "Successfully installed");
    }

    #[test]
    fn clean_winget_output_preserves_failure_reason() {
        let raw = "Found 7-Zip [7zip.7zip] Version 25.01\r\
                   - \r  / \r\
                   Installer failed with exit code: 1603";
        assert_eq!(
            clean_winget_output(raw),
            "Installer failed with exit code: 1603"
        );
    }
}
