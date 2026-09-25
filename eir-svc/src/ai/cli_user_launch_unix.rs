//! Linux non-root CLI launch (private to `cli_user`): drop privilege from root to the
//! configured `[api] linux_ai_user` before spawning the AI CLI, mirroring
//! `cli_user_launch.rs`'s Windows `CreateProcessAsUserW` shape (`UserCliSpec` /
//! `run_cli_as_active_user`) so the four provider call sites need no further changes.
//!
//! Fail-closed by construction: [`running_as_local_system`] is the real
//! `geteuid() == 0` check (replacing the old hardcoded `false`), and
//! [`run_cli_as_active_user`] refuses to launch anything unless `linux_ai_user`
//! resolves to a real, non-root local user — there is no "launch as current process
//! user" fallback path on Linux the way claude_cli.rs's `else` branch provides on
//! Windows, because when eir-svc runs as root under systemd that current-process-user
//! path IS root.
//!
//! Workspace creation is entirely fd-relative (`mkdirat`/`openat` with `O_NOFOLLOW`,
//! `fchown`/`fchmod` on the held fd), never by re-resolving a path string. `linux_ai_user`
//! owns their own `$HOME` and can replace `~/.cache` (or the scratch leaf itself, if it
//! ever raced a name reuse) with a symlink between calls; a path-based
//! `create_dir_all`+`chown` would silently follow that symlink while still running as
//! root, letting the very account the privilege drop exists to contain redirect a
//! root-owned `mkdir`+`chown` anywhere on the filesystem. Walking the chain by fd closes
//! that window: `O_NOFOLLOW` makes a symlinked component fail outright instead of being
//! followed, and every write after that point (`stdin.txt`/`stdout.txt`/`stderr.txt`,
//! extra workspace files, and final cleanup) goes through the same held fds rather than
//! a fresh path lookup.

use crate::ai::cli_process::{CliProcessOutput, CLI_OUTPUT_CAP};
use crate::ai::cli_user::UserCliSpec;
use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub(crate) fn running_as_local_system() -> bool {
    // SAFETY: geteuid() takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[derive(Debug)]
struct UnixUser {
    uid: u32,
    gid: u32,
    groups: Vec<libc::gid_t>,
    home: String,
    name: String,
}

/// Resolve `[api] linux_ai_user` to a real, non-root local account via `getpwnam_r`
/// (thread-safe; this runs off the async runtime but still inside a multi-threaded
/// process). Fails closed on every ambiguous case: unset, blank, `root`/uid 0, or a
/// name that does not resolve.
fn resolve_configured_user() -> Result<UnixUser> {
    let Some(name) = crate::session::configured_ai_user() else {
        bail!(
            "Linux AI-CLI launch refused: [api] linux_ai_user is not set in config.toml \
             (eir-svc runs as root and will not run the AI CLI as root)"
        );
    };
    let c_name = CString::new(name.as_str())
        .map_err(|_| anyhow::anyhow!("linux_ai_user contains a NUL byte"))?;

    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0i8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `pwd`/`buf` are valid, correctly sized out-parameters for the duration
    // of this one call; `result` either stays null or points into `pwd`.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        bail!("Configured linux_ai_user '{name}' does not exist on this system");
    }
    if pwd.pw_uid == 0 {
        bail!(
            "linux_ai_user '{name}' resolves to uid 0 (root) — refusing to launch the AI CLI as root"
        );
    }
    // SAFETY: getpwnam_r succeeded, so pw_dir points at a live, NUL-terminated string
    // owned by the (still valid) `pwd`/`buf` pair.
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .into_owned();
    if home.trim().is_empty() {
        bail!("linux_ai_user '{name}' has no home directory configured");
    }
    reject_dangerous_primary_group(&name, pwd.pw_gid)?;
    Ok(UnixUser {
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        // Only the primary group: every supplementary group (docker, sudo, adm, …) is
        // dropped for the AI-CLI child. See DANGEROUS_GROUPS.
        groups: vec![pwd.pw_gid],
        home,
        name,
    })
}

/// Local groups that are effectively root-equivalent: `docker` membership lets a member
/// mount the host filesystem through a privileged container, and `sudo`/`wheel`/`adm`
/// allow re-escalation or reading sensitive logs. The AI CLI runs its own shell commands,
/// so its child keeps ONLY the account's primary group — every supplementary group is
/// dropped — and the child also sets `PR_SET_NO_NEW_PRIVS`, so even a NOPASSWD sudoers
/// rule for the account cannot raise it back. An account whose PRIMARY group is one of
/// these is refused outright, because that group cannot be dropped.
const DANGEROUS_GROUPS: &[&str] = &["docker", "sudo", "wheel", "adm"];

/// Resolve a gid to its group name via `getgrgid_r` (thread-safe, mirroring
/// `getpwnam_r` above). `None` on any lookup failure — callers treat that as "not a
/// known dangerous group" rather than failing the whole launch over an unrelated gid
/// lookup problem; the explicit denylist below is the security boundary, not this.
fn group_name(gid: libc::gid_t) -> Option<String> {
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0i8; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: `grp`/`buf` are valid, correctly sized out-parameters for the duration
    // of this one call; `result` either stays null or points into `grp`.
    let rc = unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: getgrgid_r succeeded, so gr_name points at a live, NUL-terminated string
    // owned by the (still valid) `grp`/`buf` pair.
    Some(
        unsafe { std::ffi::CStr::from_ptr(grp.gr_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Fail closed — exactly like the uid-0 check above — when the configured AI user's
/// PRIMARY group is on [`DANGEROUS_GROUPS`] (supplementary groups are simply dropped).
fn reject_dangerous_primary_group(name: &str, primary_gid: libc::gid_t) -> Result<()> {
    if let Some(group) = group_name(primary_gid)
        .filter(|g| DANGEROUS_GROUPS.iter().any(|d| d.eq_ignore_ascii_case(g)))
    {
        bail!(
            "linux_ai_user '{name}' has the root-equivalent primary group '{group}' — refusing \
             to launch the AI CLI with it. Use an account whose primary group is its own."
        );
    }
    Ok(())
}

fn default_path(home: &str) -> String {
    format!(
        "{home}/.local/bin:{home}/.npm-global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    )
}

fn cstring(component: &str) -> Result<CString> {
    CString::new(component)
        .map_err(|_| anyhow::anyhow!("path component '{component}' contains a NUL byte"))
}

/// A single path component (directory or file name), never a multi-segment path — used
/// everywhere below to guarantee every `mkdirat`/`openat` call stays strictly inside the
/// fd it is given, never escaping via `/` or `..` embedded in a name.
fn require_plain_component(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("Refusing unsafe workspace path component '{name}'");
    }
    Ok(())
}

/// Open `home` itself as a directory. `home` comes from `getpwnam_r`, not from
/// attacker-influenced path text, so a single non-fd-relative open here is fine — the
/// fd-relative chain starts from this point.
fn open_home_dir(home: &str) -> Result<OwnedFd> {
    let c_home = cstring(home)?;
    // SAFETY: c_home is a valid NUL-terminated path for the duration of the call.
    let fd = unsafe {
        libc::open(
            c_home.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Open home directory '{home}' for the configured AI user"));
    }
    // SAFETY: fd was just opened successfully and is not owned elsewhere.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Step into (creating if absent) a single path component strictly inside `parent`,
/// refusing to follow a symlink substituted at that name. A freshly created directory is
/// chowned via the fd we now hold open on it — never via the path — so there is no gap
/// between "create" and "chown" for a concurrent rename/symlink-swap to land in.
/// `require_fresh` rejects reuse of a name that must be uniquely ours (the scratch leaf,
/// whose name embeds this process's own pid and a call-local sequence number — a
/// pre-existing entry there can only be a hostile collision, not a legitimate resume).
fn step_into_dir_no_follow(
    parent: &OwnedFd,
    name: &str,
    uid: u32,
    gid: u32,
    require_fresh: bool,
) -> Result<OwnedFd> {
    require_plain_component(name)?;
    let c_name = cstring(name)?;
    // SAFETY: parent.as_raw_fd() is a valid, open directory fd for this call's duration.
    let mkdir_rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) };
    let created = mkdir_rc == 0;
    if !created {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err).with_context(|| format!("Create directory component '{name}'"));
        }
        if require_fresh {
            bail!(
                "Refusing to reuse pre-existing workspace directory '{name}' — possible \
                 collision or leftover from a previous run"
            );
        }
    }
    // SAFETY: O_NOFOLLOW makes this fail with ELOOP rather than transparently follow a
    // symlink that was substituted for a real directory at this component — the actual
    // defence this function exists for.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!("Open directory component '{name}' (symlink substituted, or not a directory?)")
        });
    }
    // SAFETY: fd is freshly opened and exclusively owned from here on.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    if created {
        // SAFETY: dir.as_raw_fd() names the directory we just created and still hold
        // open by fd — this chown cannot be redirected by a later path-based swap.
        let rc = unsafe { libc::fchown(dir.as_raw_fd(), uid, gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown directory component '{name}'"));
        }
    }
    Ok(dir)
}

/// Create a file strictly inside `dir` (refusing to follow or overwrite a pre-existing
/// entry — `O_EXCL|O_NOFOLLOW`) and chown it to the target user via the held fd.
fn create_file_no_follow(dir: &OwnedFd, name: &str, uid: u32, gid: u32) -> Result<std::fs::File> {
    require_plain_component(name)?;
    let c_name = cstring(name)?;
    // SAFETY: dir.as_raw_fd() is a valid open directory fd; O_EXCL|O_NOFOLLOW refuse any
    // pre-existing file or symlink at this name rather than opening through it.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Create workspace file '{name}'"));
    }
    // SAFETY: fd was just opened successfully and is not owned elsewhere.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: file.as_raw_fd() is the fd just opened above.
    let rc = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("chown workspace file '{name}'"));
    }
    Ok(file)
}

/// Re-open an existing file strictly inside `dir` for reading, refusing to follow a
/// symlink substituted at that name after we created the real file.
fn open_file_no_follow_read(dir: &OwnedFd, name: &str) -> Result<std::fs::File> {
    require_plain_component(name)?;
    let c_name = cstring(name)?;
    // SAFETY: dir.as_raw_fd() is a valid open directory fd for this call's duration.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Open workspace file '{name}' for reading"));
    }
    // SAFETY: fd was just opened successfully and is not owned elsewhere.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Remove a single file strictly inside `dir` by name — used for the fd-relative
/// cleanup below, which never re-resolves the workspace by path.
fn unlink_no_follow(dir: &OwnedFd, name: &str) {
    let Ok(c_name) = cstring(name) else { return };
    // SAFETY: dir.as_raw_fd() is a valid open directory fd; unlinking is inherently
    // fd-relative here (no separate symlink-following step exists for unlinkat itself).
    unsafe {
        libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0);
    }
}

struct Workspace {
    /// Held open for the whole call so `run_cli_as_active_user` can create/reopen
    /// workspace files by fd-relative name instead of re-resolving a path.
    dir: OwnedFd,
    /// Parent (`~/.cache/eir`) fd plus this leaf's own name, kept only so cleanup can
    /// `unlinkat(..., AT_REMOVEDIR)` the leaf itself without a path lookup.
    parent_dir: OwnedFd,
    leaf_name: String,
    /// Informational only (passed to the CLI via `workspace_flag`, if any) — never used
    /// for a filesystem operation on our side, since `dir`/`parent_dir` already are.
    display_path: PathBuf,
    /// Names of every file created inside `dir`, tracked so cleanup can `unlinkat` each
    /// one by fd-relative name rather than reading the directory back.
    created_files: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_workspace(
    user: &UnixUser,
    scratch_prefix: &str,
    seq: u64,
    prompt: &str,
    files: &[(String, Vec<u8>)],
    workspace_files: crate::ai::cli_user::WorkspaceFiles,
) -> Result<(Workspace, std::fs::File, std::fs::File, std::fs::File)> {
    let home_dir = open_home_dir(&user.home)?;
    let cache_dir = step_into_dir_no_follow(&home_dir, ".cache", user.uid, user.gid, false)?;
    let eir_dir = step_into_dir_no_follow(&cache_dir, "eir", user.uid, user.gid, false)?;
    let leaf_name = format!("{scratch_prefix}-{}-{seq}", std::process::id());
    let workspace_dir = step_into_dir_no_follow(&eir_dir, &leaf_name, user.uid, user.gid, true)?;

    let mut created_files = Vec::new();

    let mut stdin_file = create_file_no_follow(&workspace_dir, "stdin.txt", user.uid, user.gid)?;
    created_files.push("stdin.txt".to_string());
    stdin_file.write_all(prompt.as_bytes())?;
    stdin_file.flush()?;
    stdin_file.seek(SeekFrom::Start(0))?;

    let stdout_file = create_file_no_follow(&workspace_dir, "stdout.txt", user.uid, user.gid)?;
    created_files.push("stdout.txt".to_string());
    let stderr_file = create_file_no_follow(&workspace_dir, "stderr.txt", user.uid, user.gid)?;
    created_files.push("stderr.txt".to_string());

    for (name, bytes) in files.iter().chain(workspace_files(&user.home).iter()) {
        let mut file = create_file_no_follow(&workspace_dir, name, user.uid, user.gid)
            .with_context(|| format!("Write workspace file '{name}'"))?;
        file.write_all(bytes)?;
        created_files.push(name.clone());
    }

    let display_path = Path::new(&user.home)
        .join(".cache")
        .join("eir")
        .join(&leaf_name);
    let workspace = Workspace {
        dir: workspace_dir,
        parent_dir: eir_dir,
        leaf_name,
        display_path,
        created_files,
    };
    Ok((workspace, stdin_file, stdout_file, stderr_file))
}

/// Remove every file this call created inside the workspace, then the workspace
/// directory itself — entirely fd-relative (`unlinkat` against held fds), so a
/// directory-entry swap elsewhere in `$HOME` after creation cannot redirect the cleanup
/// the way a path-based `remove_dir_all` on the reconstructed path could.
fn cleanup_workspace(workspace: &Workspace) {
    for name in &workspace.created_files {
        unlink_no_follow(&workspace.dir, name);
    }
    let Ok(c_leaf) = cstring(&workspace.leaf_name) else {
        return;
    };
    // SAFETY: parent_dir.as_raw_fd() is a valid open directory fd; AT_REMOVEDIR removes
    // the (now-empty) leaf directory itself.
    unsafe {
        libc::unlinkat(
            workspace.parent_dir.as_raw_fd(),
            c_leaf.as_ptr(),
            libc::AT_REMOVEDIR,
        );
    }
}

fn read_file_capped(dir: &OwnedFd, name: &str) -> Result<String> {
    let file = open_file_no_follow_read(dir, name)?;
    let mut bytes = Vec::new();
    file.take(CLI_OUTPUT_CAP as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn run_cli_as_active_user(
    spec: UserCliSpec<'_>,
    args: &[String],
    prompt: &str,
    files: &[(String, Vec<u8>)],
    seq: u64,
) -> Result<CliProcessOutput> {
    let UserCliSpec {
        configured_binary,
        resolve_binary,
        what,
        scratch_prefix,
        workspace_flag,
        workspace_files,
        timeout_ms,
    } = spec;

    let user = resolve_configured_user()?;
    let binary = resolve_binary(configured_binary, Some(&user.home));
    if !Path::new(&binary).is_file() {
        bail!(
            "{what} was not found for the configured Linux AI user '{}'",
            user.name
        );
    }

    let (workspace, stdin_file, stdout_file, stderr_file) =
        prepare_workspace(&user, scratch_prefix, seq, prompt, files, workspace_files)?;

    let result = (|| {
        let mut process_args = args.to_vec();
        if let Some(flag) = workspace_flag {
            process_args.push(flag.to_string());
            process_args.push(workspace.display_path.to_string_lossy().into_owned());
        }

        // File-redirected stdio (not piped): the prompt is already on disk in
        // `stdin.txt` (written by `prepare_workspace`, fd already rewound to the
        // start) and stdout/stderr redirect straight to their own files, so there is
        // no pipe-buffer deadlock to guard against and no writer thread is needed —
        // mirrors the Windows launch path's own file-redirection design
        // (`cli_user_launch.rs`'s `CliRedirections`).
        let (uid, gid, groups) = (user.uid, user.gid, user.groups.clone());
        let mut command = std::process::Command::new(&binary);
        command.args(&process_args);
        command.current_dir(&user.home);
        command.env_clear();
        command.env("HOME", &user.home);
        command.env("USER", &user.name);
        command.env("LOGNAME", &user.name);
        command.env("PATH", default_path(&user.home));
        command.stdin(Stdio::from(stdin_file));
        command.stdout(Stdio::from(stdout_file));
        command.stderr(Stdio::from(stderr_file));
        // SAFETY: the closure only calls async-signal-safe libc functions
        // (setgroups/setgid/setuid) between fork and exec, in the one order that is
        // actually valid — supplementary groups and the real/effective gid must be
        // dropped before uid, because dropping uid first removes the privilege needed
        // to change either. `Command::groups()` in std is still unstable, so this
        // does that step directly instead.
        unsafe {
            command.pre_exec(move || {
                // No setuid/setgid binary (sudo, su, pkexec) can raise this child again.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("Launch {what} as the configured Linux AI user"))?;
        let child_pid = child.id();

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(child.wait());
        });

        let status = match result_rx.recv_timeout(Duration::from_millis(u64::from(timeout_ms))) {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => return Err(error).context(format!("{what} process error")),
            Err(_timeout) => {
                // SAFETY: `child_pid` was this process's own child; killing a pid that
                // has since exited and been reused is the same inherent, accepted
                // TOCTOU race the Windows launch path's TerminateProcess shares.
                unsafe {
                    libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
                }
                bail!("{what} timed out after {}s", timeout_ms / 1_000);
            }
        };

        Ok(CliProcessOutput {
            code: status.code().map(|c| c as u32).unwrap_or(u32::MAX),
            stdout: read_file_capped(&workspace.dir, "stdout.txt")?,
            stderr: read_file_capped(&workspace.dir, "stderr.txt")?,
        })
    })();

    cleanup_workspace(&workspace);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_puts_the_users_own_bin_dirs_first() {
        let path = default_path("/home/ubuntu");
        assert!(path.starts_with("/home/ubuntu/.local/bin:/home/ubuntu/.npm-global/bin:"));
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn resolve_configured_user_fails_closed_when_unset() {
        // No config.toml at the resolved path in a unit-test sandbox — configured_ai_user()
        // returns None, and this must refuse rather than falling through to any
        // "run as current process user" path (there is none on Linux).
        let err = resolve_configured_user().unwrap_err().to_string();
        assert!(
            err.contains("linux_ai_user") || err.contains("config.toml"),
            "{err}"
        );
    }

    #[test]
    fn a_primary_group_not_on_the_denylist_is_accepted() {
        // gid 0 ("root" on virtually every Linux system) is a plausible-but-not-listed
        // group name — confirms the check is name-based, not "any privileged-looking gid".
        assert!(reject_dangerous_primary_group("test-user", 0).is_ok());
    }

    #[test]
    fn a_root_equivalent_primary_group_is_refused_when_present_on_this_host() {
        // Best-effort: only meaningful where one of DANGEROUS_GROUPS actually exists as
        // a local group (true on swatbox, and most dev boxes with docker/sudo
        // installed) — skip quietly elsewhere rather than failing on a minimal image.
        let Some(gid) = DANGEROUS_GROUPS.iter().find_map(|name| {
            let c_name = CString::new(*name).ok()?;
            let mut grp: libc::group = unsafe { std::mem::zeroed() };
            let mut buf = vec![0i8; 16 * 1024];
            let mut result: *mut libc::group = std::ptr::null_mut();
            // SAFETY: same fixed-size out-parameter contract as group_name() above.
            let rc = unsafe {
                libc::getgrnam_r(
                    c_name.as_ptr(),
                    &mut grp,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut result,
                )
            };
            (rc == 0 && !result.is_null()).then_some(grp.gr_gid)
        }) else {
            return;
        };
        let err = reject_dangerous_primary_group("test-user", gid)
            .expect_err("a root-equivalent primary group must be refused");
        assert!(err.to_string().contains("root-equivalent"), "{err}");
    }

    #[test]
    fn require_plain_component_rejects_traversal_and_separators() {
        assert!(require_plain_component("stdin.txt").is_ok());
        assert!(require_plain_component("").is_err());
        assert!(require_plain_component(".").is_err());
        assert!(require_plain_component("..").is_err());
        assert!(require_plain_component("a/b").is_err());
        assert!(require_plain_component("../escape").is_err());
    }

    /// End-to-end proof the symlink defence works: swap a directory component for a
    /// symlink to a target outside the intended tree and confirm the walk refuses it
    /// rather than creating/chowning inside the symlink target.
    #[test]
    fn step_into_dir_no_follow_refuses_a_symlinked_component() {
        let base = std::env::temp_dir().join(format!(
            "eir-symlink-poc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create test base");
        let attacker_target = base.join("attacker_target");
        std::fs::create_dir_all(&attacker_target).expect("create attacker target");
        let victim_home = base.join("victim_home");
        std::fs::create_dir_all(&victim_home).expect("create victim home");
        let cache_link = victim_home.join(".cache");
        std::os::unix::fs::symlink(&attacker_target, &cache_link).expect("create symlink");

        let home_dir = open_home_dir(&victim_home.to_string_lossy()).expect("open home");
        // SAFETY: getuid()/getgid() take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let result = step_into_dir_no_follow(&home_dir, ".cache", uid, gid, false);
        assert!(
            result.is_err(),
            "a symlinked '.cache' component must be refused, not followed"
        );
        assert!(
            std::fs::read_dir(&attacker_target)
                .expect("read attacker target")
                .next()
                .is_none(),
            "nothing should have been created inside the symlink target"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
