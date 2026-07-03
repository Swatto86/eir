use anyhow::{bail, Result};
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SERVICE_NOT_ACTIVE;
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    StartServiceW, SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP, SERVICE_STOPPED,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn restart(name: &str) -> Result<String> {
    info!(service = name, "Restarting service");
    stop(name)?;
    wait_for(name, SERVICE_STOPPED, 30)?;
    start(name)?;
    wait_for(name, SERVICE_RUNNING, 30)?;
    Ok(format!("Service '{name}' restarted successfully"))
}

pub fn stop(name: &str) -> Result<String> {
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
    result
}

pub fn start(name: &str) -> Result<String> {
    let name_w = wide(name);
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)? };

    let svc = unsafe { OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_START) };

    let result = match svc {
        Ok(h) => {
            let r = unsafe { StartServiceW(h, None) }
                .map(|_| format!("Start issued for '{name}'"))
                .map_err(|e| anyhow::anyhow!("StartServiceW failed: {e}"));
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
    result
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
