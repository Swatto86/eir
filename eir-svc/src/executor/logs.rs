use crate::policy::{is_network_path, is_within, normalize_path_lexical};
use crate::session::active_user_session_id;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{ImpersonateLoggedOnUser, RevertToSelf},
    System::RemoteDesktop::WTSQueryUserToken,
};

const CLEANABLE_EXTENSIONS: &[&str] = &["log", "tmp", "dmp", "etl", "blf", "regtrans-ms"];

struct ActiveUserImpersonation(HANDLE);

impl ActiveUserImpersonation {
    fn new() -> Result<Self> {
        let session = active_user_session_id().ok_or_else(|| {
            anyhow::anyhow!("No active desktop user is available for this file action")
        })?;
        let mut token = HANDLE::default();
        unsafe {
            WTSQueryUserToken(session, &mut token).context("Get active desktop user token")?;
            if let Err(error) = ImpersonateLoggedOnUser(token) {
                let _ = CloseHandle(token);
                return Err(error).context("Impersonate active desktop user");
            }
        }
        Ok(Self(token))
    }
}

impl Drop for ActiveUserImpersonation {
    fn drop(&mut self) {
        unsafe {
            if let Err(error) = RevertToSelf() {
                warn!(%error, "Failed to end file-action user impersonation");
            }
            let _ = CloseHandle(self.0);
        }
    }
}

pub(crate) fn with_active_user<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _user = ActiveUserImpersonation::new()?;
    f()
}

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
/// Shared with the `FileDelete` executor, which canonicalises then re-checks here.
pub(crate) fn is_protected_file(path: &str) -> bool {
    PROTECTED_DIRS.iter().any(|d| is_within(path, d))
}

pub(crate) fn checked_local_path(path: &Path) -> Result<Option<PathBuf>> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    if !path.is_absolute() {
        bail!(
            "Refusing '{}' — path is not an absolute local drive path",
            path.display()
        );
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                current.push(component.as_os_str());
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(_) => {
                current.push(component.as_os_str());
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata)
                        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 =>
                    {
                        bail!(
                            "Refusing '{}' — path contains a reparse point",
                            path.display()
                        );
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("Cannot inspect file-action path '{}'", current.display())
                        });
                    }
                }
            }
            Component::CurDir => {}
            _ => bail!(
                "Refusing '{}' — path is not a plain local drive path",
                path.display()
            ),
        }
    }

    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Cannot resolve log cleanup path '{}'", path.display()))?;
    let mut components = canonical.components();
    let local_drive = matches!(
        components.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) && matches!(components.next(), Some(Component::RootDir));
    if !local_drive {
        bail!(
            "Refusing '{}' — resolved target is not a local drive",
            path.display()
        );
    }
    Ok(Some(canonical))
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

pub fn cleanup(path: &str, days_old: u32) -> Result<String> {
    // days_old == 0 makes the cutoff "now", matching every existing file — a mass
    // delete disguised as a log cleanup. Require a real age window.
    if days_old == 0 {
        bail!("Refusing log cleanup with days_old = 0 — require at least 1 day");
    }
    // Defense in depth (policy already blocks this): never walk a UNC/network root — it
    // would make the LocalSystem account authenticate to a remote host over SMB.
    if is_network_path(path) {
        bail!("Refusing log cleanup on '{path}' — network/UNC paths are not allowed");
    }
    // Refuse a root broad enough to reach system directories.
    if root_too_broad(path) {
        bail!("Refusing log cleanup on '{path}' — root is a drive root or contains protected system directories");
    }

    // The service is LocalSystem, but these paths are user-controlled. Keep the
    // complete check/walk/delete sequence at the active user's privilege level.
    let _user = ActiveUserImpersonation::new()?;
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
    let _user = ActiveUserImpersonation::new()?;
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

    #[test]
    fn cleanup_requires_an_effect_when_work_was_attempted() {
        assert!(validate_cleanup_result(0, 1, 0, false).is_err());
        assert!(validate_cleanup_result(0, 0, 1, false).is_err());
        assert!(validate_cleanup_result(0, 0, 0, true).is_err());
        assert!(validate_cleanup_result(1, 2, 1, true).is_ok());
        assert!(validate_cleanup_result(0, 0, 0, false).is_ok());
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
