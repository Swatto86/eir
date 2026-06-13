use anyhow::{bail, Result};
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
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
            let r = unsafe { ControlService(h, SERVICE_CONTROL_STOP, &mut status) }
                .map(|_| format!("Stop signal sent to '{name}'"))
                .map_err(|e| anyhow::anyhow!("ControlService failed: {e}"));
            unsafe {
                let _ = CloseServiceHandle(h);
            }
            r
        }
        Err(e) => bail!("Cannot open service '{name}': {e}"),
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
        Err(e) => bail!("Cannot open service '{name}': {e}"),
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
    let svc = unsafe { OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_QUERY_STATUS)? };

    loop {
        if Instant::now() > deadline {
            unsafe {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(manager);
            }
            bail!("Timed out waiting for service '{name}' to reach state {target:?}");
        }
        let mut status = SERVICE_STATUS::default();
        unsafe {
            let _ = QueryServiceStatus(svc, &mut status);
        }
        if status.dwCurrentState == target {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    unsafe {
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(manager);
    }
    Ok(())
}
