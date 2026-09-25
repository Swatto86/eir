//! File-safety actions shared by `LogCleanup` and `FileDelete`: an allow-rooted,
//! canonicalize-then-recheck path guard and a protected-system-directory blocklist.
//! The path-safety primitives are platform-specific (`windows.rs` drive letters /
//! reparse points, `unix.rs` a plain root / symlinks); the orchestration below —
//! `cleanup()`, `delete_file()`, the age cutoff, the walk — is unchanged from before
//! the Linux port.

use crate::policy::is_network_path;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};
use tracing::info;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{checked_local_path, is_protected_file, root_too_broad, ActiveUserScope};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{checked_local_path, is_protected_file, root_too_broad, ActiveUserScope};

const CLEANABLE_EXTENSIONS: &[&str] = &["log", "tmp", "dmp", "etl", "blf", "regtrans-ms"];

/// Run `f` with the platform's file-action privilege scope held for its duration (the
/// Windows active-desktop-user impersonation, or a no-op on headless Linux).
pub(crate) fn with_active_user<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _user = ActiveUserScope::new()?;
    f()
}

fn validate_cleanup_result(
    deleted: u32,
    attempted: u32,
    walk_errors: u32,
    timed_out: bool,
) -> Result<()> {
    if deleted == 0 && walk_errors > 0 {
        bail!("Log cleanup could not enumerate the target ({walk_errors} read error(s))");
    }
    if deleted == 0 && attempted > 0 {
        bail!("Log cleanup could not remove any of {attempted} eligible file(s)");
    }
    if deleted == 0 && timed_out {
        bail!("Log cleanup reached its time limit before removing any file");
    }
    Ok(())
}

fn cleanable_extension(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    CLEANABLE_EXTENSIONS
        .iter()
        .any(|cleanable| cleanable.eq_ignore_ascii_case(extension))
}

pub fn cleanup(path: &str, days_old: u32) -> Result<String> {
    // days_old == 0 makes the cutoff "now", matching every existing file — a mass
    // delete disguised as a log cleanup. Require a real age window.
    if days_old == 0 {
        bail!("Refusing log cleanup with days_old = 0 — require at least 1 day");
    }
    // Defense in depth (policy already blocks this): never walk a UNC/network root — it
    // would make the service account authenticate to a remote host over SMB.
    if is_network_path(path) {
        bail!("Refusing log cleanup on '{path}' — network/UNC paths are not allowed");
    }
    // Refuse a root broad enough to reach system directories.
    if root_too_broad(path) {
        bail!("Refusing log cleanup on '{path}' — root is a drive root or contains protected system directories");
    }

    // Keep the complete check/walk/delete sequence at the platform's file-action
    // privilege scope for its duration.
    let _user = ActiveUserScope::new()?;
    let dir = Path::new(path);
    let Some(walk_root) = checked_local_path(dir)? else {
        return Ok(format!("Directory '{path}' does not exist, skipping"));
    };
    if root_too_broad(&walk_root.to_string_lossy()) {
        bail!("Refusing log cleanup on '{path}' — it resolves to a protected system location");
    }

    // Self-terminate before the executor's 10-min backstop aborts us: an abort can't reach
    // this blocking walk, so without an internal deadline it would keep deleting after the
    // UI already reported the action "abandoned". Leave a minute of margin.
    let deadline = Instant::now() + Duration::from_secs(9 * 60);

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(days_old as u64 * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut deleted = 0u32;
    let mut skipped = 0u32;
    let mut bytes_freed: u64 = 0;
    let mut timed_out = false;
    let mut walk_errors = 0u32;
    let mut attempted = 0u32;

    for entry in walkdir::WalkDir::new(&walk_root) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                walk_errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let p = entry.path();
        if !cleanable_extension(p) {
            continue;
        }

        // Never delete inside a protected system directory, even if the root looked
        // benign (e.g. reached via a symlink/junction).
        if is_protected_file(&p.to_string_lossy()) {
            skipped += 1;
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                walk_errors += 1;
                continue;
            }
        };

        let modified = match meta.modified() {
            Ok(modified) => modified,
            Err(_) => {
                walk_errors += 1;
                continue;
            }
        };
        if modified >= cutoff {
            continue;
        }

        let size = meta.len();
        attempted += 1;
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
    let note = if timed_out {
        " (stopped early at the time limit)"
    } else {
        ""
    };
    validate_cleanup_result(deleted, attempted, walk_errors, timed_out)?;
    skipped = skipped.saturating_add(walk_errors);
    Ok(format!(
        "Cleaned {deleted} files ({mb_freed:.1} MB freed), {skipped} locked/protected/skipped \
         (>{days_old} days old in '{path}'){note}"
    ))
}

pub fn delete_file(path: &str) -> Result<String> {
    if is_network_path(path) {
        bail!("Refusing to delete '{path}' — network/UNC paths are not allowed");
    }
    let _user = ActiveUserScope::new()?;
    delete_file_as_active_user(path)
}

fn delete_file_as_active_user(path: &str) -> Result<String> {
    let Some(canonical) = checked_local_path(Path::new(path))? else {
        return Ok(format!("Not found (already gone?): {path}"));
    };
    if is_protected_file(&canonical.to_string_lossy()) {
        bail!(
            "Refusing to delete '{path}': resolves into a protected system directory ({})",
            canonical.display()
        );
    }
    let metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("Cannot inspect file '{}'", canonical.display()))?;
    if metadata.is_dir() {
        bail!("Refusing to delete directory: {path}");
    }
    std::fs::remove_file(&canonical)
        .with_context(|| format!("Delete file '{}'", canonical.display()))?;
    if canonical
        .try_exists()
        .with_context(|| format!("Verify deletion of '{}'", canonical.display()))?
    {
        bail!("File still exists after deletion: {path}");
    }
    Ok(format!("Deleted: {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_days_old_zero() {
        assert!(cleanup("nonexistent-eir-log-cleanup-target", 0).is_err());
    }

    #[test]
    fn cleanup_requires_an_effect_when_work_was_attempted() {
        assert!(validate_cleanup_result(0, 1, 0, false).is_err());
        assert!(validate_cleanup_result(0, 0, 1, false).is_err());
        assert!(validate_cleanup_result(0, 0, 0, true).is_err());
        assert!(validate_cleanup_result(1, 2, 1, true).is_ok());
        assert!(validate_cleanup_result(0, 0, 0, false).is_ok());
    }

    #[test]
    fn cleanable_extensions_are_case_insensitive() {
        assert!(cleanable_extension(Path::new("old.log")));
        assert!(cleanable_extension(Path::new("old.LOG")));
        assert!(cleanable_extension(Path::new("trace.EtL")));
        assert!(!cleanable_extension(Path::new("keep.txt")));
    }

    #[test]
    fn native_file_delete_verifies_the_effect() {
        let root = std::env::temp_dir().join(format!(
            "eir-delete-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("create test directory");
        let path = root.join("old.log");
        std::fs::write(&path, b"old").expect("create test file");
        let message =
            delete_file_as_active_user(&path.to_string_lossy()).expect("delete test file");
        assert!(message.starts_with("Deleted:"));
        assert!(!path.exists());
        std::fs::remove_dir(&root).expect("remove test directory");
    }
}
