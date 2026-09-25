use crate::policy::normalize_path_lexical;
use crate::session::active_user_session_id;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf, Prefix};
use tracing::warn;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{ImpersonateLoggedOnUser, RevertToSelf},
    System::RemoteDesktop::WTSQueryUserToken,
};

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

// The field is held only for its Drop side effect (ending the impersonation) — never
// read directly, so it is not dead code despite the lint.
pub(crate) struct ActiveUserScope(#[allow(dead_code)] ActiveUserImpersonation);

impl ActiveUserScope {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self(ActiveUserImpersonation::new()?))
    }
}

/// Directories whose files must never be deleted by a log cleanup, even when they
/// match a cleanable extension — Windows keeps live ETW traces and registry
/// transaction logs here, and removing them can destabilise the OS. Matched on
/// component boundaries (shared with the policy blocklist), so a cleanup rooted at a
/// broad path can't recurse in and delete them.
const PROTECTED_DIRS: &[&[&str]] = &[
    &["windows", "system32"],
    &["windows", "syswow64"],
    &["windows", "winsxs"],
    &["windows", "boot"],
    &["windows", "fonts"],
];

fn drive_relative_components(components: &[String]) -> Option<&[String]> {
    let drive = components.first()?.as_bytes();
    (drive.len() == 2 && drive[0].is_ascii_alphabetic() && drive[1] == b':')
        .then_some(&components[1..])
}

fn components_start_with(path: &[String], prefix: &[&str]) -> bool {
    path.len() >= prefix.len()
        && path
            .iter()
            .zip(prefix)
            .all(|(component, expected)| component == expected)
}

/// A scan root is too broad if it is a bare drive/filesystem root, or an ancestor of
/// (or equal to / inside) a protected system directory. The policy layer only checks
/// the root against its path blocklist; a root like `C:\` or `C:\Windows` passes that
/// yet would recurse into `System32`, so this is the executor's own gate.
pub(crate) fn root_too_broad(path: &str) -> bool {
    let components = normalize_path_lexical(path);
    // Fewer than two components means a drive root (`C:\`) or empty — never a
    // specific-enough log location.
    if components.len() < 2 {
        return true;
    }
    let Some(relative) = drive_relative_components(&components) else {
        return false;
    };
    // Reject if the root sits inside a protected dir, OR is an ancestor of one
    // on any local drive. Windows is normally on C:, but that is not guaranteed.
    PROTECTED_DIRS.iter().any(|dir| {
        components_start_with(relative, dir)
            || (dir.len() >= relative.len()
                && dir
                    .iter()
                    .zip(relative)
                    .all(|(expected, component)| component == expected))
    })
}

/// A specific file that must not be deleted because it lives under a protected dir.
/// Belt-and-suspenders against junctions/edge roots that slip past [`root_too_broad`].
/// Shared with the `FileDelete` executor, which canonicalises then re-checks here.
pub(crate) fn is_protected_file(path: &str) -> bool {
    let components = normalize_path_lexical(path);
    drive_relative_components(&components).is_some_and(|relative| {
        PROTECTED_DIRS
            .iter()
            .any(|dir| components_start_with(relative, dir))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn protected_windows_dirs_are_not_tied_to_the_c_drive() {
        assert!(root_too_broad("D:\\Windows"));
        assert!(root_too_broad("D:\\Windows\\System32\\config"));
        assert!(is_protected_file(
            "D:\\Windows\\System32\\LogFiles\\trace.etl"
        ));
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
