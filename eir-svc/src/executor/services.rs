use anyhow::{bail, Result};
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    StartServiceW, SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP, SERVICE_STOPPED,
};

/// Services that must never be stopped/restarted by an auto-executed action — an
/// adapter-level backstop mirroring `driver.rs::CRITICAL_DRIVERS` and
/// `process.rs::PROTECTED_PROCESSES`, so a `policy.toml` blocklist edit/typo can't
/// expose them. Stopping any of these can hang or crash the interactive session. Matched
/// case-insensitively (Windows service names are case-insensitive). `start` is never
/// guarded — starting a service is not disruptive.
// Kept to services whose *stop* is genuinely catastrophic (hangs/crashes the interactive
// session or breaks core IPC), mirroring PROTECTED_PROCESSES' "never touch" intent. Note
// this deliberately excludes routinely-restartable services like Dnscache and Schedule —
// restarting the DNS client is a legitimate fix Eir may propose, so it must not be blocked.
const CRITICAL_SERVICES: &[&str] = &[
    "rpcss",
    "dcomlaunch",
    "winmgmt",
    "eventlog",
    "lsm",
    "plugplay",
    "samss",
    "brokerinfrastructure",
    "power",
    "lsass",
    // Eir must not auto-stop itself, and service actions must never weaken the
    // machine's firewall or antimalware protection even if policy.toml is edited.
    "eirsvc",
    "windefend",
    "mpssvc",
    "bfe",
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.encode_utf16().count() > 256
        || name.chars().any(char::is_control)
        || name.contains(['\\', '/'])
    {
        bail!("Invalid Windows service name");
    }
    Ok(())
}

pub fn restart(name: &str) -> Result<String> {
    info!(service = name, "Restarting service");
    stop(name)?;
    start(name)?;
    Ok(format!("Service '{name}' restarted successfully"))
}

pub fn stop(name: &str) -> Result<String> {
    validate_name(name)?;
    // Adapter-level backstop: never stop a critical service, even if policy.toml's
    // blocklist was edited to omit it. `restart` routes through here, so this covers both.
    if CRITICAL_SERVICES.contains(&name.to_lowercase().as_str()) {
        bail!("Refusing to stop critical service '{name}'");
    }
    let name_w = wide(name);
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)? };

    let svc = unsafe {
        OpenServiceW(
            manager,
            PCWSTR(name_w.as_ptr()),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        )
    };

    let result = match svc {
        Ok(h) => {
            let mut status = SERVICE_STATUS::default();
            // A service that is already stopped — the usual state of a *failed*
            // service — returns ERROR_SERVICE_NOT_ACTIVE. Treat that as success so
            // `restart` (stop → wait → start) still proceeds to start it, instead of
            // bailing at the `stop(name)?` and never restarting the thing.
            let r = match unsafe { ControlService(h, SERVICE_CONTROL_STOP, &mut status) } {
                Ok(_) => Ok(format!("Stop signal sent to '{name}'")),
                Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => {
                    Ok(format!("Service '{name}' was already stopped"))
                }
                Err(e) => Err(anyhow::anyhow!("ControlService failed: {e}")),
            };
            unsafe {
                let _ = CloseServiceHandle(h);
            }
            r
        }
        Err(e) => Err(anyhow::anyhow!("Cannot open service '{name}': {e}")),
    };

    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result?;
    wait_for(name, SERVICE_STOPPED, 30)?;
    Ok(format!("Service '{name}' is stopped"))
}

pub fn start(name: &str) -> Result<String> {
    validate_name(name)?;
    let name_w = wide(name);
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)? };

    let svc = unsafe { OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_START) };

    let result = match svc {
        Ok(h) => {
            let r = match unsafe { StartServiceW(h, None) } {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => Ok(()),
                Err(e) => Err(anyhow::anyhow!("StartServiceW failed: {e}")),
            };
            unsafe {
                let _ = CloseServiceHandle(h);
            }
            r
        }
        Err(e) => Err(anyhow::anyhow!("Cannot open service '{name}': {e}")),
    };

    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result?;
    wait_for(name, SERVICE_RUNNING, 30)?;
    Ok(format!("Service '{name}' is running"))
}

fn wait_for(name: &str, target: SERVICE_STATUS_CURRENT_STATE, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let name_w = wide(name);
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)? };

    let svc = match unsafe { OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_QUERY_STATUS) }
    {
        Ok(h) => h,
        Err(e) => {
            unsafe {
                let _ = CloseServiceHandle(manager);
            }
            return Err(anyhow::anyhow!("Cannot open service '{name}': {e}"));
        }
    };

    let mut consecutive_errs = 0u32;
    loop {
        if Instant::now() > deadline {
            unsafe {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(manager);
            }
            bail!("Timed out waiting for service '{name}' to reach state {target:?}");
        }
        let mut status = SERVICE_STATUS::default();
        match unsafe { QueryServiceStatus(svc, &mut status) } {
            Ok(()) => {
                consecutive_errs = 0;
                if status.dwCurrentState == target {
                    break;
                }
            }
            // Don't silently spin to the timeout on a persistent query failure —
            // surface the real error after a few consecutive failures.
            Err(e) => {
                consecutive_errs += 1;
                if consecutive_errs >= 5 {
                    unsafe {
                        let _ = CloseServiceHandle(svc);
                        let _ = CloseServiceHandle(manager);
                    }
                    bail!("QueryServiceStatus failed for '{name}': {e}");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    unsafe {
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(manager);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{start, stop, CRITICAL_SERVICES};

    #[test]
    fn critical_services_are_refused_before_any_scm_call() {
        // The guard runs before OpenSCManagerW, so this bails without touching the SCM.
        for name in ["RpcSs", "rpcss", "EventLog", "Winmgmt", "DcomLaunch"] {
            let err = stop(name).unwrap_err().to_string();
            assert!(err.contains("critical"), "{name}: {err}");
        }
    }

    #[test]
    fn self_and_security_services_are_critical_targets() {
        for name in ["EirSvc", "WinDefend", "MpsSvc", "BFE"] {
            assert!(
                CRITICAL_SERVICES
                    .iter()
                    .any(|critical| critical.eq_ignore_ascii_case(name)),
                "{name} must never be stopped by an auto-approved service action"
            );
        }
    }

    #[test]
    fn invalid_service_names_are_refused_before_any_scm_call() {
        for name in [
            "",
            "RpcSs\0junk",
            "bad\nname",
            r"folder\service",
            "bad/name",
        ] {
            assert!(stop(name).unwrap_err().to_string().contains("Invalid"));
            assert!(start(name).unwrap_err().to_string().contains("Invalid"));
        }
    }
}
