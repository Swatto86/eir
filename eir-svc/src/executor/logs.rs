use crate::policy::{is_within, normalize_path_lexical};
use anyhow::{bail, Result};
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::info;

const CLEANABLE_EXTENSIONS: &[&str] = &["log", "tmp", "dmp", "etl", "blf", "regtrans-ms"];

/// Directories whose files must never be deleted by a log cleanup, even when they
/// match a cleanable extension — Windows keeps live ETW traces and registry
/// transaction logs here, and removing them can destabilise the OS. Matched on
/// component boundaries (shared with the policy blocklist), so a cleanup rooted at a
/// broad path can't recurse in and delete them.
const PROTECTED_DIRS: &[&str] = &[
    "C:\\Windows\\System32",
    "C:\\Windows\\SysWOW64",
    "C:\\Windows\\WinSxS",
    "C:\\Windows\\Boot",
    "C:\\Windows\\Fonts",
];

/// A scan root is too broad if it is a bare drive/filesystem root, or an ancestor of
/// (or equal to / inside) a protected system directory. The policy layer only checks
/// the root against its path blocklist; a root like `C:\` or `C:\Windows` passes that
/// yet would recurse into `System32`, so this is the executor's own gate.
fn root_too_broad(path: &str) -> bool {
    // Fewer than two components means a drive root (`C:\`) or empty — never a
    // specific-enough log location.
    if normalize_path_lexical(path).len() < 2 {
        return true;
    }
    // Reject if the root sits inside a protected dir, OR is an ancestor of one
    // (`is_within(protected, root)` means the root contains that protected dir).
    PROTECTED_DIRS
        .iter()
        .any(|d| is_within(path, d) || is_within(d, path))
}

/// A specific file that must not be deleted because it lives under a protected dir.
/// Belt-and-suspenders against junctions/edge roots that slip past [`root_too_broad`].
fn is_protected_file(path: &str) -> bool {
    PROTECTED_DIRS.iter().any(|d| is_within(path, d))
}

pub fn cleanup(path: &str, days_old: u32) -> Result<String> {
    // days_old == 0 makes the cutoff "now", matching every existing file — a mass
    // delete disguised as a log cleanup. Require a real age window.
    if days_old == 0 {
        bail!("Refusing log cleanup with days_old = 0 — require at least 1 day");
    }
    // Refuse a root broad enough to reach system directories.
    if root_too_broad(path) {
        bail!("Refusing log cleanup on '{path}' — root is a drive root or contains protected system directories");
    }

    let dir = Path::new(path);
    if !dir.exists() {
        return Ok(format!("Directory '{path}' does not exist, skipping"));
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(days_old as u64 * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut deleted = 0u32;
    let mut skipped = 0u32;
    let mut bytes_freed: u64 = 0;

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = entry.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !CLEANABLE_EXTENSIONS.contains(&ext) {
            continue;
        }

        // Never delete inside a protected system directory, even if the root looked
        // benign (e.g. reached via a junction).
        if is_protected_file(&p.to_string_lossy()) {
            skipped += 1;
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if modified >= cutoff {
            continue;
        }

        let size = meta.len();
        match std::fs::remove_file(p) {
            Ok(()) => {
                deleted += 1;
                bytes_freed += size;
                info!(path = %p.display(), "Deleted old log file");
            }
            Err(_) => skipped += 1,
        }
    }

    let mb_freed = bytes_freed as f64 / (1024.0 * 1024.0);
    Ok(format!(
        "Cleaned {deleted} files ({mb_freed:.1} MB freed), {skipped} locked/protected/skipped \
         (>{days_old} days old in '{path}')"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_days_old_zero() {
        assert!(cleanup("C:\\ProgramData\\App\\logs", 0).is_err());
    }

    #[test]
    fn rejects_broad_roots() {
        assert!(root_too_broad("C:\\"));
        assert!(root_too_broad("C:"));
        assert!(root_too_broad("\\"));
        // The Windows tree is an ancestor of protected dirs → too broad.
        assert!(root_too_broad("C:\\Windows"));
        // Inside a protected dir → refused.
        assert!(root_too_broad("C:\\Windows\\System32\\config"));
        // slash + case forms
        assert!(root_too_broad("c:/windows/system32"));
        // The exact exploit from the plan.
        assert!(cleanup("C:\\", 0).is_err());
        assert!(cleanup("C:\\Windows", 7).is_err());
    }

    #[test]
    fn allows_specific_log_dirs() {
        assert!(!root_too_broad("C:\\ProgramData\\SomeApp\\logs"));
        assert!(!root_too_broad("C:\\Windows\\Logs")); // a real, safe log location
        assert!(!root_too_broad("C:\\Users\\me\\AppData\\Local\\App\\logs"));
    }

    #[test]
    fn protects_files_under_system_dirs() {
        assert!(is_protected_file(
            "C:\\Windows\\System32\\LogFiles\\trace.etl"
        ));
        assert!(is_protected_file("c:/windows/winsxs/x.log"));
        assert!(!is_protected_file("C:\\ProgramData\\App\\logs\\app.log"));
    }
}
