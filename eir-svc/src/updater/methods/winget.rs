//! The winget update method. Runs directly as SYSTEM (no `Start-Process -Verb
//! RunAs`, no UAC — the service is already elevated), captures winget's own output
//! so a failure carries its real reason, retries once with `--force` only for the
//! self-updating-portable case, and verifies the version actually moved. The output
//! cleaning was ported verbatim with its tests.

use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::methods::detect;
use crate::updater::proc::{self, INSTALL, LIST};
use crate::updater::verify::{verify_app, VerifyTarget};
use crate::updater::winget_parse::{parse_upgrades, AppUpdate};

/// List apps with an available update (`winget upgrade`). Runs unprivileged-style
/// (listing needs no special rights) and parses the fixed-width table.
pub async fn list_updates() -> Result<Vec<AppUpdate>, String> {
    let (code, out) = run_winget(
        vec![
            "upgrade".to_string(),
            "--include-unknown".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        LIST,
    )
    .await;
    proc::checked_output("winget update listing", code, &out)?;
    parse_update_listing("winget update listing", &out)
}

pub(crate) fn parse_update_listing(action: &str, output: &str) -> Result<Vec<AppUpdate>, String> {
    // The table separator is locale-independent, but the parser's column labels are
    // English. A table without those labels is incomplete coverage, not "up to date".
    if output.contains("---") && !crate::updater::winget_parse::header_present(output) {
        return Err(format!(
            "{action} returned a table whose header could not be parsed (possibly localized)"
        ));
    }
    Ok(parse_upgrades(output))
}

/// The full `winget upgrade` argument list for one id, optionally forcing past the
/// portable-integrity check. `exact` adds `--exact`; it must be OFF when the id is a
/// truncated prefix (winget truncates long ids in the `winget upgrade` listing), since
/// `--exact` disables the prefix/substring matching that would otherwise resolve it.
fn upgrade_args(id: &str, force: bool, exact: bool) -> Vec<String> {
    let mut a = vec!["upgrade".to_string(), "--id".to_string(), id.to_string()];
    if exact {
        a.push("--exact".to_string());
    }
    a.extend([
        "--silent".to_string(),
        "--accept-package-agreements".to_string(),
        "--accept-source-agreements".to_string(),
        "--disable-interactivity".to_string(),
    ]);
    if force {
        a.push("--force".to_string());
    }
    a
}

/// winget could not resolve the id to an installed/available package — the signature
/// of a truncated `--id` used with `--exact`. Deliberately NOT "no available upgrade":
/// that means winget DID resolve the package and it is already current, so re-running
/// without `--exact` only burns a second install-length run and risks prefix-matching
/// a different package.
fn no_package_found(output: &str) -> bool {
    let l = output.to_lowercase();
    l.contains("no package found") || l.contains("no installed package")
}

/// Run winget directly and capture its merged output. The service is SYSTEM, so no
/// elevation wrapper is needed; winget also suppresses its live progress bar when
/// stdout isn't a console, which keeps the capture clean. Shared with the Store
/// method (which is winget with `--source msstore`). Resolves the real exe path
/// because LocalSystem has no winget alias in PATH.
pub(crate) async fn run_winget(args: Vec<String>, dur: std::time::Duration) -> (i32, String) {
    match detect::winget_path_async().await {
        Some(p) if winget_should_use_active_user(crate::ai::client::running_as_local_system()) => {
            crate::ai::client::run_winget_as_active_user(&p, &args, dur)
                .await
                .unwrap_or_else(|error| {
                    (
                        -1,
                        format!("could not run winget as the active desktop user: {error}"),
                    )
                })
        }
        Some(p) => proc::run_capped(&p.to_string_lossy(), &args, dur).await,
        None => proc::run_capped("winget", &args, dur).await,
    }
}

fn winget_should_use_active_user(running_as_local_system: bool) -> bool {
    running_as_local_system
}

/// winget refused because a portable package's files changed after install — its
/// documented remedy is to re-run with --force. Self-updating CLIs (e.g. the GitHub
/// Copilot CLI) trip this on every upgrade.
fn portable_modified(output: &str) -> bool {
    let l = output.to_lowercase();
    l.contains("has been modified") && l.contains("--force")
}

/// Attempt to update one app via winget, then verify the version moved. Auto-retries
/// once with --force for the portable-modified case; `force_first` runs the very
/// first upgrade with `--force` (used when the AI diagnostician requests the Force
/// remedy).
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

    let (mut code, mut output) = run_winget(upgrade_args(&id, force_first, true), INSTALL).await;
    // Only auto-escalate to --force when we didn't already start with it.
    if !force_first && code != 0 && portable_modified(&output) {
        let (c, o) = run_winget(upgrade_args(&id, true, true), INSTALL).await;
        code = c;
        output = o;
    }
    // If --exact found nothing, the id is likely a truncated prefix from the upgrade
    // listing; retry with prefix matching (no --exact) so long-id apps still update.
    if code != 0 && no_package_found(&output) {
        let (c, o) = run_winget(upgrade_args(&id, force_first, false), INSTALL).await;
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
    fn already_current_does_not_trigger_the_prefix_retry() {
        assert!(no_package_found(
            "No installed package found matching input criteria."
        ));
        assert!(no_package_found(
            "No package found matching input criteria."
        ));
        // Resolved but current — retrying without --exact would upgrade nothing at
        // best, and a prefix-matched neighbour at worst.
        assert!(!no_package_found("No available upgrade found."));
    }

    #[test]
    fn upgrade_args_appends_force_only_when_asked() {
        let plain = upgrade_args("GitHub.Copilot", false, true);
        assert_eq!(plain.first().map(String::as_str), Some("upgrade"));
        assert!(plain.iter().any(|a| a == "GitHub.Copilot"));
        assert!(!plain.iter().any(|a| a == "--force"));
        assert!(upgrade_args("GitHub.Copilot", true, true)
            .iter()
            .any(|a| a == "--force"));
    }

    #[test]
    fn upgrade_args_exact_flag_is_optional() {
        // A truncated-prefix id must be upgradable WITHOUT --exact (prefix matching).
        assert!(
            upgrade_args("Microsoft.VisualStudio.2022.Buil", false, true)
                .iter()
                .any(|a| a == "--exact")
        );
        assert!(
            !upgrade_args("Microsoft.VisualStudio.2022.Buil", false, false)
                .iter()
                .any(|a| a == "--exact")
        );
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

    #[test]
    fn localized_table_is_not_an_empty_success() {
        let output = "Nom        Identifiant  Version  Disponible  Source\n\
                      ---------- ----------- -------- ----------- ------\n\
                      Example    Vendor.App  1.0      2.0         winget\n";
        let error = parse_update_listing("winget update listing", output)
            .expect_err("an unparseable table must be reported");
        assert!(error.contains("header"));
    }

    #[test]
    fn local_system_uses_the_active_users_winget_catalog() {
        assert!(winget_should_use_active_user(true));
        assert!(!winget_should_use_active_user(false));
    }
}
