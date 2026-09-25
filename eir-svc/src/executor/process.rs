use anyhow::{bail, Result};

#[cfg(windows)]
const PROTECTED_PROCESSES: &[&str] = &[
    "System", "Idle", "lsass", "winlogon", "csrss", "smss", "wininit", "services", "svchost",
    "ntoskrnl", "eir-svc", "MsMpEng",
];

/// Linux processes whose exact `/proc/<pid>/comm` name must never be killed — the
/// same "adapter-level backstop" shape as the Windows list above, covering init,
/// login/session and core security/remote-access daemons plus Eir itself.
#[cfg(unix)]
const PROTECTED_PROCESSES: &[&str] = &[
    "systemd",
    "init",
    "sshd",
    "dbus-daemon",
    "systemd-journal",
    "systemd-logind",
    "tailscaled",
    "eir-svc",
    "cron",
    "auditd",
];

#[cfg(windows)]
pub async fn kill(process_name: &str) -> Result<String> {
    validate_kill_target(process_name)?;
    // A Windows process's Name has no extension, so Get-Process -Name never matches a
    // ".exe"-suffixed string. Strip it (validate_kill_target strips it for the blocklist)
    // before building the query.
    let safe_name = process_query_name(process_name).replace('\'', "''");
    // Report success only if a process actually existed and was stopped. The old script
    // used `-ErrorAction SilentlyContinue` and unconditionally wrote success, so a typo'd/
    // absent/hallucinated name or an access-denied kill was logged as a fabricated success
    // (and suppressed retries via safety::rate_limited). Now: throw if nothing matches, and
    // stop with `-ErrorAction Stop` so a genuine failure surfaces as a non-zero exit.
    // Every interpolation of the name stays inside single-quoted PS literals.
    let script = format!(
        "$procs = @(Get-Process -Name '{safe_name}' -ErrorAction SilentlyContinue); \
         if ($procs.Count -eq 0) {{ throw 'No running process named: {safe_name}' }}; \
         $ids = @($procs.Id); \
         $procs | Stop-Process -Force -ErrorAction Stop; \
         foreach ($id in $ids) {{ Wait-Process -Id $id -Timeout 10 -ErrorAction Stop }}; \
         Write-Output 'Stopped process(es) named: {safe_name}'"
    );
    super::powershell::run_diagnostic(&script).await
}

/// Exact-name kill via `SIGKILL` — no wildcards, no pattern expansion, mirroring
/// `validate_kill_target`'s guarantees on Windows. Matches on `/proc/<pid>/comm`, the
/// same short (kernel-truncated to 15 bytes) name `pgrep -x`/`killall` use, so a
/// hallucinated or typo'd name reports failure rather than a fabricated success.
#[cfg(unix)]
pub async fn kill(process_name: &str) -> Result<String> {
    validate_kill_target(process_name)?;
    let name = process_name.to_string();
    let pids = tokio::task::spawn_blocking(move || unix_find_pids_by_exact_name(&name))
        .await
        .map_err(|e| anyhow::anyhow!("Process scan task panicked: {e}"))??;
    if pids.is_empty() {
        bail!("No running process named: {process_name}");
    }
    for pid in &pids {
        // SAFETY: `pid` was just read from a live /proc entry. Killing a pid that has
        // since exited and been reused by an unrelated process is an inherent,
        // accepted TOCTOU race shared by every "kill by name" tool, including
        // Windows' own Stop-Process -Name.
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && pids.iter().any(|pid| process_alive(*pid)) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    let still_alive: Vec<i32> = pids.into_iter().filter(|pid| process_alive(*pid)).collect();
    if !still_alive.is_empty() {
        bail!("Process(es) named '{process_name}' did not exit within 10s: {still_alive:?}");
    }
    Ok(format!("Stopped process(es) named: {process_name}"))
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(unix)]
fn unix_find_pids_by_exact_name(name: &str) -> Result<Vec<i32>> {
    let entries =
        std::fs::read_dir("/proc").map_err(|e| anyhow::anyhow!("Cannot read /proc: {e}"))?;
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid_str) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim_end_matches('\n') == name {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// The name to pass to `Get-Process -Name`: a Windows process Name carries no
/// extension, so a trailing ".exe" (any case) is removed. UTF-8 safe (splits on '.').
#[cfg(windows)]
pub(crate) fn process_query_name(process_name: &str) -> &str {
    process_name
        .rsplit_once('.')
        .filter(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
        .map(|(stem, _)| stem)
        .unwrap_or(process_name)
}

#[cfg(windows)]
pub(crate) fn validate_kill_target(process_name: &str) -> Result<()> {
    // Stop-Process -Name glob-expands `*?[]` even inside a single-quoted literal
    // (globbing is cmdlet-level, not string interpolation), so `lsass*` would match
    // and kill lsass without ever equalling the exact-match blocklist entry. Refuse
    // wildcards up front — mirrors the has_glob_meta guard in tasks.rs/registry.rs.
    if process_name.contains(['*', '?', '[', ']']) {
        bail!("Process name '{process_name}' contains wildcard characters — refusing");
    }
    let lower = process_name.to_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    if PROTECTED_PROCESSES
        .iter()
        .any(|protected| protected.eq_ignore_ascii_case(lower))
    {
        bail!("Refusing to kill protected process: {process_name}");
    }
    Ok(())
}

/// Linux process names (`/proc/<pid>/comm`) are plain strings with no wildcard
/// expansion involved in how we match them — the wildcard refusal here exists purely
/// so an AI-hallucinated glob-looking name fails loudly instead of matching nothing
/// silently, mirroring the Windows guard's shape. Matching is case-sensitive (Linux
/// process names are); `PROTECTED_PROCESSES` entries are already lowercase/exact.
#[cfg(unix)]
pub(crate) fn validate_kill_target(process_name: &str) -> Result<()> {
    if process_name.contains(['*', '?', '[', ']']) {
        bail!("Process name '{process_name}' contains wildcard characters — refusing");
    }
    if process_name.is_empty() || process_name.len() > 255 || process_name.contains(['/', '\0']) {
        bail!("Invalid process name: {process_name}");
    }
    if PROTECTED_PROCESSES.contains(&process_name) {
        bail!("Refusing to kill protected process: {process_name}");
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[tokio::test]
    async fn wildcard_process_names_are_refused() {
        // A glob would let Stop-Process match many processes (incl. a protected one
        // whose exact name isn't equal to the pattern) — reject before building a script.
        for bad in ["chrome*", "lsass*", "csrss?", "note[p]ad", "*"] {
            let err = kill(bad).await.unwrap_err();
            assert!(err.to_string().contains("wildcard"), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn protected_processes_are_refused() {
        for name in [
            "lsass",
            "lsass.exe",
            "SVCHOST.EXE",
            "eir-svc.exe",
            "MSMPENG.EXE",
        ] {
            let err = kill(name).await.unwrap_err();
            assert!(err.to_string().contains("protected"), "{err}");
        }
    }

    #[test]
    fn process_query_name_strips_exe_so_get_process_matches() {
        assert_eq!(process_query_name("chrome.exe"), "chrome");
        assert_eq!(process_query_name("Chrome.EXE"), "Chrome");
        assert_eq!(process_query_name("chrome"), "chrome");
        assert_eq!(process_query_name("my.app.exe"), "my.app");
    }

    #[test]
    fn shared_service_hosts_and_eir_are_protected_targets() {
        for name in ["svchost", "eir-svc", "MsMpEng"] {
            assert!(
                PROTECTED_PROCESSES
                    .iter()
                    .any(|protected| protected.eq_ignore_ascii_case(name)),
                "{name} must never be killed"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[tokio::test]
    async fn wildcard_process_names_are_refused() {
        for bad in ["chrome*", "sshd*", "cron?", "note[p]ad", "*"] {
            let err = kill(bad).await.unwrap_err();
            assert!(err.to_string().contains("wildcard"), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn protected_processes_are_refused_before_any_signal_is_sent() {
        for name in ["systemd", "sshd", "tailscaled", "eir-svc", "cron", "auditd"] {
            let err = kill(name).await.unwrap_err();
            assert!(err.to_string().contains("protected"), "{err}");
        }
    }

    #[tokio::test]
    async fn a_nonexistent_process_name_is_reported_not_silently_successful() {
        let err = kill("eir-definitely-not-a-real-process-xyz")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No running process"), "{err}");
    }

    #[test]
    fn find_pids_by_exact_name_matches_this_test_processes_own_name() {
        // The test binary's own process is a stable, real, harmless target: proves
        // the /proc scan + exact-match logic works without signalling anything.
        let pid = std::process::id() as i32;
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .expect("read own comm")
            .trim_end_matches('\n')
            .to_string();
        let pids = unix_find_pids_by_exact_name(&comm).expect("scan /proc");
        assert!(
            pids.contains(&pid),
            "own pid must be found by its own comm name"
        );
    }

    #[test]
    fn shared_daemons_and_eir_are_protected_targets() {
        for name in ["sshd", "eir-svc", "tailscaled"] {
            assert!(
                PROTECTED_PROCESSES.contains(&name),
                "{name} must never be killed"
            );
        }
    }
}
