//! Scoop is user-scoped and its `.cmd` shim is user-writable. EirSvc runs as SYSTEM,
//! so this backend is fail-closed until it has a real active-user process launcher.

use crate::updater::domain::{AttemptOutcome, ErrorCategory, Method, UpdateCandidate};
#[cfg(test)]
use crate::updater::version::is_newer;

const DISABLED_REASON: &str =
    "Scoop is disabled in the SYSTEM service because its user-owned shim cannot be run safely";

/// One outdated Scoop app.
pub struct ScoopUpdate {
    pub name: String,
    pub current: String,
    pub available: String,
}

/// A Scoop app identifier is a slug: `app` or `bucket/app`, using only alphanumerics
/// and `.`, `-`, `_`, `/`. Anything else is rejected before it reaches `cmd.exe`. A
/// leading `-` is refused too: a real scoop slug never starts with a dash, but a name
/// like `--all` would be re-read by scoop's OWN argv parser as a flag (`scoop update
/// --all` = update everything), bypassing the per-app/budget scoping — a different
/// vector from the shell-metacharacter defense the char set already covers.
#[cfg(test)]
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
#[cfg(test)]
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

/// Listing is disabled rather than executing a user-owned shim as SYSTEM.
pub async fn list_outdated() -> Result<Vec<ScoopUpdate>, String> {
    Err(DISABLED_REASON.to_string())
}

/// Updating is disabled for the same privilege-boundary reason as listing.
pub async fn attempt(_candidate: &UpdateCandidate) -> AttemptOutcome {
    AttemptOutcome::failed(Method::Scoop, ErrorCategory::Blocked, DISABLED_REASON)
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

    #[tokio::test]
    async fn system_backend_never_selects_or_executes_a_user_owned_shim() {
        match list_outdated().await {
            Err(reason) => assert_eq!(reason, DISABLED_REASON),
            Ok(_) => panic!("Scoop unexpectedly became available to the SYSTEM service"),
        }
        let candidate = UpdateCandidate {
            id: "attacker-scoop-shim".to_string(),
            name: r"C:\Users\attacker\scoop\shims\scoop.cmd".to_string(),
            current: "1".to_string(),
            available: "2".to_string(),
            package_id: Some(r"C:\Users\attacker\scoop\shims\scoop.cmd".to_string()),
            methods: vec![Method::Scoop],
        };
        let outcome = attempt(&candidate).await;
        assert!(!outcome.success);
        assert_eq!(outcome.category, Some(ErrorCategory::Blocked));
        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.detail, DISABLED_REASON);
    }
}
