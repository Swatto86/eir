//! The Scoop update method. Scoop is user-scoped, so the service runs it in the
//! logged-in user's profile context (USERPROFILE/HOME pointed at their home, like it
//! borrows their Claude session) — best-effort: scoop also relies on the user's PATH
//! (git etc.), which a SYSTEM service can't fully reproduce. We never bootstrap scoop
//! as SYSTEM. Verification is by exit code (scoop apps don't register in ARP).

use crate::updater::domain::{
    classify_error, AttemptOutcome, ErrorCategory, Method, UpdateCandidate, Verification,
};
use crate::updater::methods::detect;
use crate::updater::proc::{self, INSTALL, LIST};
use crate::updater::version::is_newer;
use std::path::PathBuf;
use std::time::Duration;

/// One outdated Scoop app.
pub struct ScoopUpdate {
    pub name: String,
    pub current: String,
    pub available: String,
}

/// Run a scoop command via its .cmd shim in the user's profile context. Bounded by
/// `dur`: scoop shells out to git/network, so a stall must not wedge the cycle. (The
/// timeout kills the `cmd` shim; a grandchild git/scoop process may briefly linger.)
async fn run_scoop(
    profile: String,
    shim: PathBuf,
    args: Vec<String>,
    dur: Duration,
) -> (i32, String) {
    let homepath = profile.strip_prefix("C:").unwrap_or(&profile).to_string();
    let mut cmd = std::process::Command::new("cmd");
    cmd.arg("/c")
        .arg(&shim)
        .args(&args)
        .env("USERPROFILE", &profile)
        .env("HOME", &profile)
        .env("HOMEDRIVE", "C:")
        .env("HOMEPATH", homepath);
    proc::run_capped_cmd(cmd, dur).await
}

/// A Scoop app identifier is a slug: `app` or `bucket/app`, using only alphanumerics
/// and `.`, `-`, `_`, `/`. Anything else is rejected before it reaches `cmd.exe`. A
/// leading `-` is refused too: a real scoop slug never starts with a dash, but a name
/// like `--all` would be re-read by scoop's OWN argv parser as a flag (`scoop update
/// --all` = update everything), bypassing the per-app/budget scoping — a different
/// vector from the shell-metacharacter defense the char set already covers.
fn is_safe_scoop_name(app: &str) -> bool {
    !app.is_empty()
        && !app.starts_with('-')
        && app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
}

/// Parse `scoop status`: a whitespace-aligned table whose first three columns are
/// Name, Installed Version, Latest Version. Only rows where a strictly newer Latest
/// exists are kept. Scoop app names are slugs (no spaces), so splitting on whitespace
/// is safe.
fn parse_status(text: &str) -> Vec<ScoopUpdate> {
    let mut out = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            if trimmed.starts_with("----") {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let (name, installed, latest) = (cols[0], cols[1], cols[2]);
        if is_newer(latest, installed) {
            out.push(ScoopUpdate {
                name: name.to_string(),
                current: installed.to_string(),
                available: latest.to_string(),
            });
        }
    }
    out
}

/// List outdated Scoop apps (runs in the user's context).
pub async fn list_outdated() -> Vec<ScoopUpdate> {
    let Some((profile, shim)) = detect::scoop_install() else {
        return Vec::new();
    };
    let (_code, out) = run_scoop(profile, shim, vec!["status".to_string()], LIST).await;
    parse_status(&out)
}

/// Update one app via Scoop. Success is exit-code based (scoop apps aren't in ARP, so
/// there's no independent version to verify against).
pub async fn attempt(candidate: &UpdateCandidate) -> AttemptOutcome {
    let Some((profile, shim)) = detect::scoop_install() else {
        return AttemptOutcome::failed(
            Method::Scoop,
            ErrorCategory::NotFound,
            "Scoop is not installed for any user",
        );
    };
    let app = candidate
        .package_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| candidate.name.clone());

    // Defense-in-depth: the shim runs via `cmd /c <shim> update <app>`, and because the
    // program is `cmd` (not the .cmd shim itself) Rust's batch-argument escaping does not
    // engage — so `&`, `|`, `^`, `"`, etc. in `app` would reach cmd.exe's re-parser.
    // Real scoop names are slugs (`app` or `bucket/app`), so anything else is refused.
    if !is_safe_scoop_name(&app) {
        return AttemptOutcome::failed(
            Method::Scoop,
            ErrorCategory::NotFound,
            format!("refusing scoop app name with unexpected characters: {app}"),
        );
    }

    let (code, output) = run_scoop(
        profile,
        shim,
        vec!["update".to_string(), app.clone()],
        INSTALL,
    )
    .await;
    let mut out = AttemptOutcome::failed(Method::Scoop, ErrorCategory::Unknown, String::new());
    out.exit_code = Some(code);
    if code == 0 {
        out.success = true;
        out.verification = Verification::Unverified;
        out.detail = format!("scoop updated {app}");
    } else {
        out.category = Some(classify_error(Method::Scoop, Some(code), &output));
        out.detail = {
            let tail: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            tail.iter()
                .rev()
                .take(3)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ")
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_keeps_only_outdated_rows() {
        let text = "Scoop is up to date.\n\
                    Name   Installed Version Latest Version Missing Dependencies Info\n\
                    ----   ----------------- -------------- -------------------- ----\n\
                    git    2.43.0            2.45.1\n\
                    neovim 0.9.5             0.9.5\n\
                    ripgrep 14.0.0           14.1.0\n";
        let ups = parse_status(text);
        let names: Vec<&str> = ups.iter().map(|u| u.name.as_str()).collect();
        // git and ripgrep have newer latest; neovim is current.
        assert_eq!(names, vec!["git", "ripgrep"]);
        assert_eq!(ups[0].current, "2.43.0");
        assert_eq!(ups[0].available, "2.45.1");
    }

    #[test]
    fn scoop_name_validation_allows_slugs_rejects_metacharacters() {
        assert!(is_safe_scoop_name("git"));
        assert!(is_safe_scoop_name("extras/vscode"));
        assert!(is_safe_scoop_name("some-app_1.2"));
        assert!(!is_safe_scoop_name(""));
        assert!(!is_safe_scoop_name("git & calc"));
        assert!(!is_safe_scoop_name("app|evil"));
        assert!(!is_safe_scoop_name("a\"b"));
        assert!(!is_safe_scoop_name("app^x"));
        // A flag-shaped name would be re-parsed by scoop as a flag, not an app.
        assert!(!is_safe_scoop_name("--all"));
        assert!(!is_safe_scoop_name("-g"));
    }
}
