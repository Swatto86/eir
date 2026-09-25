//! Linux path-safety primitives for log_cleanup/file_delete: an allow-rooted,
//! canonicalize-then-recheck symlink defence (mirroring the Windows reparse-point
//! guard) and a protected-directory blocklist for the machine's own system paths.

use crate::policy::normalize_path_lexical;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Headless Linux runs log_cleanup/file_delete as whatever local user eir-svc itself
/// is configured to run as (typically root, under systemd) — there is no separate
/// interactive desktop user to switch into for the duration of the action, unlike
/// Windows' LocalSystem-to-active-user impersonation. A real scope type is kept (rather
/// than deleting the call sites) so `cleanup()`/`delete_file()` in `mod.rs` stay
/// identical on both platforms.
pub(crate) struct ActiveUserScope;

impl ActiveUserScope {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// Directories whose files must never be deleted by a log cleanup or file_delete, even
/// when they match a cleanable extension — the machine's own package/boot/binary trees.
/// Matched on component boundaries (shared with the policy blocklist), so a cleanup
/// rooted at a broad path can't recurse in and delete them.
const PROTECTED_DIRS: &[&[&str]] = &[
    &["etc"],
    &["boot"],
    &["usr"],
    &["bin"],
    &["sbin"],
    &["lib"],
    &["lib32"],
    &["lib64"],
    &["root"],
    &["var", "lib", "dpkg"],
    &["var", "lib", "docker"],
];

fn components_start_with(path: &[String], prefix: &[&str]) -> bool {
    path.len() >= prefix.len()
        && path
            .iter()
            .zip(prefix)
            .all(|(component, expected)| component == expected)
}

/// A scan root is too broad if it is the filesystem root, or an ancestor of (or equal
/// to / inside) a protected system directory.
pub(crate) fn root_too_broad(path: &str) -> bool {
    let components = normalize_path_lexical(path);
    if components.is_empty() {
        return true;
    }
    PROTECTED_DIRS.iter().any(|dir| {
        components_start_with(&components, dir)
            || (dir.len() >= components.len()
                && dir
                    .iter()
                    .zip(&components)
                    .all(|(expected, component)| component == expected))
    })
}

/// A specific file that must not be deleted because it lives under a protected dir.
/// Belt-and-suspenders against symlinks/edge roots that slip past [`root_too_broad`].
pub(crate) fn is_protected_file(path: &str) -> bool {
    let components = normalize_path_lexical(path);
    PROTECTED_DIRS
        .iter()
        .any(|dir| components_start_with(&components, dir))
}

/// Walk every path component checking `symlink_metadata().is_symlink()` (never follow
/// a symlink component before the target is confirmed), then canonicalize and re-check
/// the result is absolute. Mirrors the Windows reparse-point guard with the drive-letter
/// requirement dropped (a plain leading `/` is Unix's only "local drive").
pub(crate) fn checked_local_path(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_absolute() {
        bail!(
            "Refusing '{}' — path is not an absolute local path",
            path.display()
        );
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(_) => {
                current.push(component.as_os_str());
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.is_symlink() => {
                        bail!("Refusing '{}' — path contains a symlink", path.display());
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
                "Refusing '{}' — path is not a plain local path",
                path.display()
            ),
        }
    }

    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Cannot resolve log cleanup path '{}'", path.display()))?;
    if !canonical.is_absolute() {
        bail!(
            "Refusing '{}' — resolved target is not a local path",
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
        assert!(root_too_broad("/"));
        assert!(root_too_broad("/etc"));
        assert!(root_too_broad("/etc/ssh"));
        assert!(root_too_broad("/var/lib/dpkg"));
        assert!(root_too_broad("/var/lib/docker/overlay2"));
    }

    #[test]
    fn allows_specific_log_dirs() {
        assert!(!root_too_broad("/var/log/eir"));
        assert!(!root_too_broad("/var/log/nginx"));
        assert!(!root_too_broad("/tmp/eir-scratch"));
    }

    #[test]
    fn protects_files_under_system_dirs() {
        assert!(is_protected_file("/etc/passwd"));
        assert!(is_protected_file("/boot/vmlinuz"));
        assert!(is_protected_file("/var/lib/dpkg/status"));
        assert!(!is_protected_file("/var/log/eir/eir.log"));
    }
}
