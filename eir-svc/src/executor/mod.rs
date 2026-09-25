// Windows-only repair families: on Linux their FixActions are refused by
// `linux_unsupported` (and blocklisted in policy.linux.toml), so they are not compiled.
#[cfg(windows)]
pub mod boot;
#[cfg(windows)]
pub mod driver;
pub mod logs;
pub mod powershell;
pub mod process;
#[cfg(windows)]
pub mod registry;
/// Registry resets only exist on Windows. The shared decision loop still names the undo
/// path; on Linux no registry undo record can ever be created, so it cannot be reached.
#[cfg(not(windows))]
pub mod registry {
    pub async fn restore_value(_undo: &crate::models::RegistryUndo) -> anyhow::Result<String> {
        anyhow::bail!("Registry undo is only available on Windows")
    }
}
pub mod repair;
#[cfg(windows)]
pub mod security;
pub mod services;
#[cfg(windows)]
pub mod software;
pub mod startup;
#[cfg(windows)]
pub mod tasks;

use crate::models::{ExecutionResult, FixAction};
use tracing::{error, info};

#[cfg(windows)]
fn build_disk_cleanup_script(target: &str) -> Option<String> {
    let (subdir, label) = match target.to_ascii_lowercase().as_str() {
        "temp" | "tmp" => ("Temp", "Temp folder"),
        "prefetch" => ("Prefetch", "Prefetch"),
        _ => return None,
    };
    let subdir = powershell::ps_single_quote(subdir);
    Some(format!(
        "$ErrorActionPreference='Stop'; $root=Join-Path $env:SystemRoot {subdir}; \
         if (-not (Test-Path -LiteralPath $root -PathType Container -ErrorAction Stop)) {{ \
           Write-Output '{label} is already empty'; return \
         }}; \
         $items=@(Get-ChildItem -LiteralPath $root -Force -ErrorAction Stop); \
         $removed=0; $skipped=0; \
         foreach($item in $items) {{ \
           try {{ \
             Remove-Item -LiteralPath $item.FullName -Recurse -Force -ErrorAction Stop; \
             if (Test-Path -LiteralPath $item.FullName -ErrorAction Stop) {{ \
               throw 'item remained after deletion' \
             }}; \
             $removed++ \
           }} catch {{ $skipped++ }} \
         }}; \
         if ($items.Count -gt 0 -and $removed -eq 0) {{ \
           throw '{label} cleanup could not remove any item' \
         }}; \
         Write-Output ('{label} cleaned: {{0}} removed, {{1}} skipped' -f $removed,$skipped)"
    ))
}

/// Linux `disk_cleanup` — real mechanism per target, matching what the Linux system
/// prompt (`ai/prompt.rs`) tells the model to send and what README.md documents. Not a
/// PowerShell script (there is no `powershell.exe` on Linux): direct `Command::new`
/// invocations, no shell, mirroring every other Linux executor module's pattern.
#[cfg(unix)]
fn linux_disk_cleanup(target: &str) -> anyhow::Result<String> {
    match target.to_ascii_lowercase().as_str() {
        "apt" => run_simple("apt-get", &["clean"])
            .map(|out| format!("apt-get clean completed{}", suffix(&out))),
        "journal" => run_simple("journalctl", &["--vacuum-time=7d"])
            .map(|out| format!("journald vacuum completed{}", suffix(&out))),
        "tmp" => purge_tmp_dirs(),
        other => anyhow::bail!("Unknown disk cleanup target: '{other}'"),
    }
}

#[cfg(unix)]
fn suffix(out: &str) -> String {
    if out.is_empty() {
        String::new()
    } else {
        format!(": {out}")
    }
}

/// Run an external command with no shell involved and no untrusted interpolation
/// (`target` is already matched against a fixed set of literals above, never passed
/// through as an argument).
#[cfg(unix)]
fn run_simple(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("Cannot run {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        anyhow::bail!(
            "{program} {} failed (exit {}): {detail}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        );
    }
}

#[cfg(unix)]
const TMP_PURGE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

#[cfg(unix)]
fn purge_tmp_dirs() -> anyhow::Result<String> {
    purge_dirs(&["/tmp", "/var/tmp"], TMP_PURGE_MAX_AGE)
}

/// Age-bounded purge of the given directories: only top-level regular files older than
/// `max_age` are removed. Deliberately conservative — never recurses into
/// subdirectories (both `/tmp` and `/var/tmp` are shared across every service on the
/// host, so blindly walking into e.g. another service's working directory is not
/// "cleanup", it's a different, much riskier action) and never follows or deletes
/// symlinks (a symlink entry is skipped outright, not dereferenced — `DirEntry::metadata`
/// already behaves like `lstat`, so a symlinked "file" is correctly identified and left
/// alone here).
#[cfg(unix)]
fn purge_dirs(roots: &[&str], max_age: std::time::Duration) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now();
    let mut removed = 0u64;
    let mut skipped = 0u64;
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue; // e.g. /var/tmp absent, or unreadable — best-effort, not fatal.
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                skipped += 1;
                continue;
            };
            if !metadata.is_file() {
                // Directories and symlinks are left alone — see the doc comment above.
                continue;
            }
            let age = metadata
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok());
            if age.is_none_or(|age| age < max_age) {
                continue; // too young (or unreadable mtime) — not our business.
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(_) => skipped += 1, // e.g. owned by another user — best-effort.
            }
        }
    }
    Ok(format!(
        "tmp cleanup: {removed} file(s) removed, {skipped} skipped (top-level only, older than {}d)",
        max_age.as_secs() / 86_400
    ))
}

#[cfg(windows)]
fn network_diagnostic_script(command: &str) -> Option<&'static str> {
    match command.to_ascii_lowercase().as_str() {
        "flush_dns" => Some(
            "ipconfig /flushdns; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig flushdns failed' }",
        ),
        "release_renew" => Some(
            "ipconfig /release; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig release failed' }; \
             Start-Sleep -Seconds 2; ipconfig /renew; \
             if ($LASTEXITCODE -ne 0) { throw 'ipconfig renew failed' }",
        ),
        "reset_tcp" => Some(
            "netsh int ip reset; \
             if ($LASTEXITCODE -ne 0) { throw 'TCP/IP reset failed' }",
        ),
        "reset_winsock" => Some(
            "netsh winsock reset; \
             if ($LASTEXITCODE -ne 0) { throw 'Winsock reset failed' }",
        ),
        _ => None,
    }
}

pub async fn execute(action: &FixAction) -> ExecutionResult {
    info!("Executing: {action:?}");

    match action {
        FixAction::ServiceRestart { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::restart(&n)).await
        }
        FixAction::ServiceStop { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::stop(&n)).await
        }
        FixAction::ServiceStart { service_name } => {
            let n = service_name.clone();
            blocking(action, move || services::start(&n)).await
        }
        FixAction::LogCleanup { path, days_old } => {
            let (p, d) = (path.clone(), *days_old);
            blocking(action, move || logs::cleanup(&p, d)).await
        }
        #[cfg(windows)]
        FixAction::DiskCleanup { target } => {
            let script = match build_disk_cleanup_script(target) {
                Some(script) => script,
                None => {
                    // Report a real failure, not a success-shaped no-op — disk_cleanup is
                    // auto-executed, so a false "success" here would poison the rate limiter
                    // (safety::rate_limited suppresses a fingerprint that already "succeeded").
                    // Mirrors the NetworkDiagnostic unknown-command branch below.
                    let msg = format!("Unknown disk cleanup target: '{target}'");
                    error!("{msg}");
                    return ExecutionResult {
                        action: format!("{action:?}"),
                        success: false,
                        output: msg,
                        undo: None,
                    };
                }
            };
            make_result(action, powershell::run_diagnostic(&script).await)
        }
        #[cfg(unix)]
        FixAction::DiskCleanup { target } => {
            let t = target.clone();
            blocking(action, move || linux_disk_cleanup(&t)).await
        }
        #[cfg(windows)]
        FixAction::PowerShellDiagnostic { script } => {
            make_result(action, powershell::run_diagnostic(script).await)
        }
        #[cfg(windows)]
        FixAction::TaskDisable { task_name } => {
            make_result(action, tasks::disable(task_name).await)
        }
        #[cfg(windows)]
        FixAction::TaskEnable { task_name } => make_result(action, tasks::enable(task_name).await),
        #[cfg(windows)]
        FixAction::RegistryReset {
            key_path,
            value_name,
            value_data,
        } => match registry::reset_value(key_path, value_name, value_data).await {
            Ok((msg, undo)) => {
                let mut res = make_result(action, Ok(msg));
                res.undo = undo;
                res
            }
            Err(e) => make_result(action, Err(e)),
        },
        #[cfg(windows)]
        FixAction::NetworkDiagnostic { command } => {
            let script = match network_diagnostic_script(command) {
                Some(script) => script,
                None => {
                    let msg = format!("Unknown network diagnostic command: '{command}'");
                    error!("{msg}");
                    return ExecutionResult {
                        action: format!("{action:?}"),
                        success: false,
                        output: msg,
                        undo: None,
                    };
                }
            };
            make_result(action, powershell::run_diagnostic(script).await)
        }
        #[cfg(windows)]
        FixAction::DriverDisable { driver_name } => {
            let n = driver_name.clone();
            make_result(action, driver::disable(&n).await)
        }
        #[cfg(windows)]
        FixAction::DriverEnable { driver_name } => {
            let n = driver_name.clone();
            make_result(action, driver::enable(&n).await)
        }
        #[cfg(windows)]
        FixAction::SoftwareUninstall { package_name } => {
            let n = package_name.clone();
            make_result(action, software::uninstall(&n).await)
        }
        #[cfg(windows)]
        FixAction::BcdEdit { element, value } => {
            let (el, val) = (element.clone(), value.clone());
            make_result(action, boot::bcd_edit(&el, &val).await)
        }
        FixAction::ProcessKill { process_name } => {
            let n = process_name.clone();
            make_result(action, process::kill(&n).await)
        }
        #[cfg(windows)]
        FixAction::FirewallEnable { profile } => {
            let p = profile.clone();
            make_result(action, security::firewall_enable(&p).await)
        }
        #[cfg(windows)]
        FixAction::DefenderSignatureUpdate => {
            make_result(action, security::defender_signature_update().await)
        }
        #[cfg(windows)]
        FixAction::DefenderRealtimeEnable => {
            make_result(action, security::defender_realtime_enable().await)
        }
        #[cfg(windows)]
        FixAction::SfcScan => make_result(action, repair::sfc_scan().await),
        #[cfg(windows)]
        FixAction::DismRestoreHealth => make_result(action, repair::dism_restore_health().await),
        #[cfg(windows)]
        FixAction::StartupSet {
            name,
            location,
            hive,
            enable,
        } => make_result(
            action,
            startup::set_enabled(name, location, hive, *enable).await,
        ),
        FixAction::FileDelete { path } => {
            let path = path.clone();
            blocking(action, move || logs::delete_file(&path)).await
        }
        // No real Linux mechanism exists for any of these 15 (see `linux_unsupported`'s
        // own doc comment) — `policy.linux.toml`'s blocklist refuses them before
        // `execute()` is ever reached for a normal proposal, and the Linux system
        // prompt never offers them, but a hand-crafted or hallucinated proposal that
        // somehow gets here still gets a coded, immediate refusal instead of running
        // into Windows-only code (`powershell.exe`, the registry, scheduled tasks, …)
        // that does not exist on this platform.
        #[cfg(unix)]
        FixAction::PowerShellDiagnostic { .. }
        | FixAction::TaskDisable { .. }
        | FixAction::TaskEnable { .. }
        | FixAction::RegistryReset { .. }
        | FixAction::NetworkDiagnostic { .. }
        | FixAction::DriverDisable { .. }
        | FixAction::DriverEnable { .. }
        | FixAction::SoftwareUninstall { .. }
        | FixAction::BcdEdit { .. }
        | FixAction::FirewallEnable { .. }
        | FixAction::DefenderSignatureUpdate
        | FixAction::DefenderRealtimeEnable
        | FixAction::SfcScan
        | FixAction::DismRestoreHealth
        | FixAction::StartupSet { .. } => linux_unsupported(action),
    }
}

/// `FixAction` variants with no real Linux mechanism — the 15 counted in
/// ARCHITECTURE.md's "Policy and fix-set scope" section and hard-blocklisted by name in
/// `policy.linux.toml`'s `blocklist.actions`. This is the second, compiled-in layer of
/// "defense in depth" those two places (and `ai::prompt`'s Linux system prompt doc
/// comment) already describe: previously they were the ONLY layer, because the match
/// arms above compiled unconditionally on Linux and would have fallen through into
/// Windows-oriented code that shells out to a nonexistent `powershell.exe` — an
/// accidental runtime ENOENT, not a designed refusal. Every arm that reaches this
/// function is now `#[cfg(unix)]`-gated at the call site above, so this really is
/// compiled out entirely on Windows and cannot regress silently.
#[cfg(unix)]
fn linux_unsupported(action: &FixAction) -> ExecutionResult {
    let msg = format!("{action:?} is not supported on Linux");
    error!("{msg}");
    ExecutionResult {
        action: format!("{action:?}"),
        success: false,
        output: msg,
        undo: None,
    }
}

async fn blocking(
    action: &FixAction,
    f: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
) -> ExecutionResult {
    let r = tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("Task panicked: {e}")));
    make_result(action, r)
}

fn make_result(action: &FixAction, r: anyhow::Result<String>) -> ExecutionResult {
    let label = format!("{action:?}");
    match r {
        Ok(msg) => {
            info!(action = %label, output = %msg, "Execution succeeded");
            ExecutionResult {
                action: label,
                success: true,
                output: msg,
                undo: None,
            }
        }
        Err(e) => {
            error!(action = %label, error = %e, "Execution failed");
            ExecutionResult {
                action: label,
                success: false,
                output: e.to_string(),
                undo: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::network_diagnostic_script;

    #[cfg(windows)]
    #[test]
    fn disk_cleanup_script_fails_when_native_effects_fail() {
        let cleanup = super::build_disk_cleanup_script("temp").expect("known target");
        assert!(cleanup.contains("$env:SystemRoot"));
        assert!(!cleanup.contains("C:\\Windows"));
        assert!(cleanup.contains("Test-Path"));
        assert!(cleanup.contains("$removed -eq 0"));
        assert!(!cleanup.contains("SilentlyContinue"));
    }

    #[cfg(windows)]
    #[test]
    fn network_diagnostic_scripts_fail_when_native_effects_fail() {
        for command in ["flush_dns", "release_renew", "reset_tcp", "reset_winsock"] {
            assert!(network_diagnostic_script(command)
                .expect("known command")
                .contains("$LASTEXITCODE"));
        }
    }

    #[cfg(unix)]
    mod linux_disk_cleanup {
        use super::super::{linux_disk_cleanup, purge_dirs};
        use std::time::{Duration, SystemTime};

        #[test]
        fn unknown_target_is_rejected() {
            let err = linux_disk_cleanup("bogus").unwrap_err().to_string();
            assert!(err.contains("Unknown disk cleanup target"), "{err}");
        }

        #[test]
        fn purge_dirs_removes_only_old_top_level_files_never_dirs_or_symlinks() {
            let base = std::env::temp_dir().join(format!(
                "eir-purge-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&base).expect("create test base");

            let old_file = base.join("old.tmp");
            std::fs::write(&old_file, b"old").expect("write old file");
            let old_time = SystemTime::now() - Duration::from_secs(30 * 86_400);
            std::fs::File::options()
                .write(true)
                .open(&old_file)
                .expect("open old file")
                .set_modified(old_time)
                .expect("backdate old file");

            let fresh_file = base.join("fresh.tmp");
            std::fs::write(&fresh_file, b"fresh").expect("write fresh file");

            let old_dir = base.join("old_dir");
            std::fs::create_dir_all(&old_dir).expect("create old dir");

            let result = purge_dirs(
                &[base.to_str().expect("utf8 path")],
                Duration::from_secs(86_400),
            )
            .expect("purge succeeds");
            assert!(result.contains("1 file(s) removed"), "{result}");

            assert!(!old_file.exists(), "old file must be removed");
            assert!(fresh_file.exists(), "fresh file must survive");
            assert!(old_dir.exists(), "directories are never touched");

            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
