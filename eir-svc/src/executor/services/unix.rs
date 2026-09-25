//! Linux service control via `systemctl`, with a two-layer protected-units backstop
//! (mirroring `CRITICAL_SERVICES`'s "adapter-level backstop... even if policy.toml was
//! edited"):
//!
//!   - **Layer 1** — compiled, generic-systemd names and glob prefixes that guard
//!     *stop*/*restart* on every Linux host (never `start` — starting a unit is not
//!     disruptive, matching the Windows side's own stated rationale).
//!   - **Layer 2** — host-specific, additive-only glob patterns loaded from
//!     `/etc/eir/protected-units.d/*.conf` (one glob per line), so an operator can
//!     protect this host's own app services without a binary rebuild. Ships with NO
//!     files in that directory by default: app services must stay restartable once a
//!     human approves the action (the empty Linux auto-execute whitelist is the real
//!     safety net — see `policy.linux.toml`).

use anyhow::{bail, Result};
use std::process::Command;
use std::time::{Duration, Instant};

/// Layer 1: exact unit names, matched case-sensitively (systemd unit names are
/// filesystem-based and case-sensitive, unlike Windows service names).
const PROTECTED_EXACT: &[&str] = &[
    "ssh.service",
    "sshd.service", // defensive alias; only ssh.service is the real Debian/Ubuntu unit
    "tailscaled.service",
    "dbus.service",
    "polkit.service",
    "cron.service",
    "auditd.service",
    "systemd-journald.service",
    "systemd-logind.service",
    "systemd-networkd.service",
    "systemd-resolved.service",
    "systemd-timesyncd.service",
    "systemd-udevd.service",
    "systemd-hostnamed.service",
    "networkd-dispatcher.service",
    "docker.service",
    "containerd.service",
    "snapd.service",
    "qemu-guest-agent.service",
    "eir.service", // self
];

/// Layer 1 glob prefixes.
const PROTECTED_GLOBS: &[&str] = &["systemd-*", "user@*", "getty@*", "serial-getty@*"];

/// Simple, dependency-free glob: `*` matches any run of characters (including none),
/// everything else must match literally. Sufficient for the prefix/suffix patterns
/// used here and in the host drop-in file.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(pc), Some(tc)) if pc == tc => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Layer 2: one glob pattern per non-empty, non-`#`-comment line across every
/// `*.conf` file directly under the protected-units directory. Missing directory (the
/// shipped default) is not an error — it just means no host-specific entries.
fn host_protected_globs() -> Vec<String> {
    let dir = crate::config::resolve("protected-units.d");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut globs = Vec::new();
    let mut files: Vec<_> = entries.flatten().collect();
    files.sort_by_key(std::fs::DirEntry::path);
    for entry in files {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("conf") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            globs.push(line.to_string());
        }
    }
    globs
}

/// True if `name` must never be stopped/restarted by an auto-executed OR
/// human-approved action — checked before every `systemctl stop`/`restart` call, and
/// re-checked defensively even though the Linux policy default requires approval for
/// every service action in the first place.
pub(crate) fn is_protected(name: &str) -> bool {
    PROTECTED_EXACT.contains(&name)
        || PROTECTED_GLOBS.iter().any(|g| glob_match(g, name))
        || host_protected_globs().iter().any(|g| glob_match(g, name))
}

/// Systemd unit names: no path separators, no control characters, no glob
/// metacharacters (which would otherwise let a crafted name match unrelated units via
/// `systemctl`'s own globbing of bare arguments), bounded length.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 256
        || name.chars().any(char::is_control)
        || name.contains(['/', '\\'])
        || name.contains(['*', '?', '[', ']'])
    {
        bail!("Invalid systemd unit name");
    }
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<String> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("Cannot run systemctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "systemctl {} failed (exit {}): {detail}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        );
    }
}

fn is_active(name: &str) -> Option<bool> {
    let output = Command::new("systemctl")
        .args(["is-active", name])
        .output()
        .ok()?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(state == "active")
}

fn wait_for_active(name: &str, want_active: bool, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if Instant::now() > deadline {
            bail!(
                "Timed out waiting for unit '{name}' to reach {}",
                if want_active { "active" } else { "inactive" }
            );
        }
        if is_active(name) == Some(want_active) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn stop(name: &str) -> Result<String> {
    validate_name(name)?;
    if is_protected(name) {
        bail!("Refusing to stop protected unit '{name}'");
    }
    run_systemctl(&["stop", name])?;
    wait_for_active(name, false, 30)?;
    Ok(format!("Unit '{name}' is stopped"))
}

pub fn start(name: &str) -> Result<String> {
    validate_name(name)?;
    // start is never guarded — starting a unit is not disruptive, matching the
    // Windows side's own stated rationale (see the module doc comment).
    run_systemctl(&["start", name])?;
    wait_for_active(name, true, 30)?;
    Ok(format!("Unit '{name}' is running"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_units_are_refused_before_any_systemctl_call() {
        // The guard runs before `run_systemctl`, so this bails without invoking it.
        for name in [
            "ssh.service",
            "sshd.service",
            "tailscaled.service",
            "docker.service",
            "eir.service",
        ] {
            let err = stop(name).unwrap_err().to_string();
            assert!(err.contains("protected"), "{name}: {err}");
        }
    }

    #[test]
    fn glob_prefixes_cover_the_documented_families() {
        for name in [
            "systemd-journald.service",
            "systemd-resolved.service",
            "user@1000.service",
            "getty@tty1.service",
            "serial-getty@ttyS0.service",
        ] {
            assert!(is_protected(name), "{name} must match a layer-1 glob");
        }
        assert!(!is_protected("caddy.service"));
    }

    #[test]
    fn glob_match_supports_prefix_and_infix_stars() {
        assert!(glob_match("systemd-*", "systemd-journald.service"));
        assert!(!glob_match("systemd-*", "my-systemd-thing.service"));
        assert!(glob_match("*.service", "caddy.service"));
        assert!(glob_match("user@*", "user@1000.service"));
        assert!(!glob_match("user@*", "userxservice"));
    }

    #[test]
    fn invalid_unit_names_are_refused_before_any_systemctl_call() {
        for name in [
            "",
            "a/b.service",
            "a\\b.service",
            "bad\nname",
            "wild*card.service",
            "quest?ion.service",
        ] {
            assert!(stop(name).unwrap_err().to_string().contains("Invalid"));
            assert!(start(name).unwrap_err().to_string().contains("Invalid"));
        }
    }

    #[test]
    fn start_is_never_guarded_by_the_protected_list() {
        // A fake name that matches the "systemd-*" protected glob but names no real
        // unit: start() must reach systemctl (and fail there with "not found"/"no
        // such unit", never touching any real service) rather than being refused up
        // front by the protected-units check — proving that check is wired into
        // stop()/restart() only, never start(). Never exercised against a real unit
        // (e.g. ssh.service) here, so this can never disrupt the host it runs on.
        let fake = "systemd-eir-nonexistent-test-unit.service";
        assert!(is_protected(fake), "fixture must match the systemd-* glob");
        let err = start(fake).unwrap_err().to_string();
        assert!(
            !err.contains("protected"),
            "start() must not consult the protected-units list: {err}"
        );
    }

    /// Real systemd check (root only): the exact start/stop/restart path an approved fix
    /// uses, on a throwaway runtime unit under /run (gone on reboot), plus protected-unit
    /// refusal that never touches the protected unit.
    /// `sudo <test binary> --ignored real_systemd`
    #[test]
    #[ignore = "needs root and systemd"]
    fn real_systemd_restart_and_protected_refusal() {
        let unit = "eir-selftest.service";
        let path = std::path::Path::new("/run/systemd/system").join(unit);
        std::fs::write(
            &path,
            "[Unit]
Description=Eir self-test (throwaway)
[Service]
ExecStart=/bin/sleep infinity
",
        )
        .expect("write runtime unit (run as root)");
        run_systemctl(&["daemon-reload"]).expect("daemon-reload");
        let outcome = (|| -> Result<()> {
            start(unit)?;
            assert_eq!(is_active(unit), Some(true));
            super::super::restart(unit)?;
            assert_eq!(is_active(unit), Some(true));
            stop(unit)?;
            assert_eq!(is_active(unit), Some(false));
            Ok(())
        })();
        let _ = run_systemctl(&["stop", unit]);
        let _ = std::fs::remove_file(&path);
        let _ = run_systemctl(&["daemon-reload"]);
        outcome.expect("start/restart/stop a throwaway unit");
        for protected in ["ssh.service", "tailscaled.service"] {
            let err = stop(protected).expect_err("protected unit must be refused");
            assert!(err.to_string().contains("protected"), "{err}");
        }
        assert_eq!(
            is_active("ssh.service"),
            Some(true),
            "ssh was never touched"
        );
    }
}
