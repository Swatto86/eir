use crate::models::{NetworkInterface, SystemState};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersInfo, IP_ADAPTER_INFO};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, ENUM_SERVICE_STATUS_PROCESSW,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_RUNNING,
    SERVICE_WIN32,
};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};

pub type SharedState = Arc<Mutex<Option<SystemState>>>;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn get_uptime_secs() -> u64 {
    unsafe { GetTickCount64() / 1000 }
}

/// Gets CPU usage via WMI PowerShell query. Runs on a blocking thread — the subprocess latency is fine.
fn get_cpu_usage() -> f32 {
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            "(Get-WmiObject Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average",
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<f32>()
            .unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

fn get_memory() -> (f32, f32) {
    let mut mem = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GlobalMemoryStatusEx(&mut mem);
    }
    let usage = mem.dwMemoryLoad as f32;
    let available_gb = mem.ullAvailPhys as f32 / (1024.0 * 1024.0 * 1024.0);
    (usage, available_gb)
}

fn get_disk() -> (f32, f32) {
    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let path = wide("C:\\");
    unsafe {
        let _ = GetDiskFreeSpaceExW(
            PCWSTR(path.as_ptr()),
            None,
            Some(&mut total_bytes),
            Some(&mut free_bytes),
        );
    }
    if total_bytes == 0 {
        return (0.0, 0.0);
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let usage = (used as f32 / total_bytes as f32) * 100.0;
    let free_gb = free_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
    (usage, free_gb)
}

fn get_services() -> (usize, Vec<String>) {
    let manager = match unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE)
    } {
        Ok(h) => h,
        Err(_) => return (0, vec![]),
    };

    let mut bytes_needed: u32 = 0;
    let mut services_returned: u32 = 0;
    let mut resume_handle: u32 = 0;

    unsafe {
        let _ = EnumServicesStatusExW(
            manager,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_ACTIVE,
            None,
            &mut bytes_needed,
            &mut services_returned,
            Some(&mut resume_handle),
            PCWSTR::null(),
        );
    }

    if bytes_needed == 0 {
        unsafe {
            let _ = CloseServiceHandle(manager);
        }
        return (0, vec![]);
    }

    let mut buf = vec![0u8; bytes_needed as usize];
    resume_handle = 0;

    let result = unsafe {
        EnumServicesStatusExW(
            manager,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_ACTIVE,
            Some(&mut buf),
            &mut bytes_needed,
            &mut services_returned,
            Some(&mut resume_handle),
            PCWSTR::null(),
        )
    };

    let mut running = 0usize;
    let mut failed: Vec<String> = Vec::new();

    if result.is_ok() {
        let records = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
                services_returned as usize,
            )
        };
        for svc in records {
            running += 1;
            if svc.ServiceStatusProcess.dwCurrentState != SERVICE_RUNNING {
                let name = unsafe {
                    let ptr = svc.lpServiceName.0;
                    let mut len = 0;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    OsString::from_wide(std::slice::from_raw_parts(ptr, len))
                        .to_string_lossy()
                        .to_string()
                };
                failed.push(name);
            }
        }
    }

    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    (running, failed)
}

fn i8_array_to_string(arr: &[i8]) -> String {
    let bytes: Vec<u8> = arr.iter().map(|&b| b as u8).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn get_network_interfaces() -> Vec<NetworkInterface> {
    let mut interfaces = Vec::new();
    let mut buf_size: u32 = 16384;
    let mut buf = vec![0u8; buf_size as usize];

    let result = unsafe {
        GetAdaptersInfo(
            Some(buf.as_mut_ptr() as *mut IP_ADAPTER_INFO),
            &mut buf_size,
        )
    };

    if result != 0 {
        return interfaces;
    }

    let mut adapter_ptr = buf.as_ptr() as *const IP_ADAPTER_INFO;
    while !adapter_ptr.is_null() {
        let adapter = unsafe { &*adapter_ptr };
        let name = i8_array_to_string(&adapter.Description);
        let ip = i8_array_to_string(&adapter.IpAddressList.IpAddress.String);
        let ipv4 = if ip == "0.0.0.0" || ip.is_empty() {
            None
        } else {
            Some(ip)
        };
        interfaces.push(NetworkInterface {
            name,
            status: if ipv4.is_some() { "up" } else { "down" }.to_string(),
            ipv4,
        });
        adapter_ptr = adapter.Next;
    }

    interfaces
}

fn get_windows_update_status() -> String {
    let key_path = wide(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\Results\\Install",
    );
    let value_name = wide("LastSuccessTime");

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return "unknown".to_string();
        }

        let mut data = vec![0u8; 128];
        let mut data_len = data.len() as u32;

        let ok = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(data.as_mut_ptr()),
            Some(&mut data_len),
        )
        .is_ok();

        let _ = RegCloseKey(hkey);

        if !ok || data_len < 2 {
            return "unknown".to_string();
        }

        let chars: &[u16] = std::slice::from_raw_parts(
            data.as_ptr() as *const u16,
            (data_len as usize / 2).saturating_sub(1),
        );
        format!("last_install: {}", String::from_utf16_lossy(chars))
    }
}

fn snapshot_state() -> SystemState {
    let uptime_secs = get_uptime_secs();
    let cpu_usage_percent = get_cpu_usage();
    let (memory_usage_percent, memory_available_gb) = get_memory();
    let (disk_usage_percent, disk_free_gb) = get_disk();
    let (running_services_count, failed_services) = get_services();
    let network_interfaces = get_network_interfaces();
    let windows_update_status = get_windows_update_status();

    SystemState {
        uptime_secs,
        cpu_usage_percent,
        memory_usage_percent,
        memory_available_gb,
        disk_usage_percent,
        disk_free_gb,
        running_services_count,
        failed_services,
        network_interfaces,
        network_errors: 0,
        disk_health: "unknown".to_string(),
        windows_update_status,
    }
}

pub fn spawn(poll_interval_secs: u64) -> (SharedState, watch::Sender<()>) {
    let shared: SharedState = Arc::new(Mutex::new(None));
    let shared_clone = shared.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match tokio::task::spawn_blocking(snapshot_state).await {
                        Ok(s) => {
                            info!(
                                cpu = s.cpu_usage_percent,
                                mem = s.memory_usage_percent,
                                disk_free_gb = s.disk_free_gb,
                                failed_services = s.failed_services.len(),
                                update = %s.windows_update_status,
                                "WMI snapshot"
                            );
                            if let Ok(mut guard) = shared_clone.lock() {
                                *guard = Some(s);
                            }
                        }
                        Err(e) => warn!("WMI snapshot task panicked: {e}"),
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    (shared, shutdown_tx)
}

pub fn current(shared: &SharedState) -> SystemState {
    shared
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| SystemState {
            uptime_secs: 0,
            cpu_usage_percent: 0.0,
            memory_usage_percent: 0.0,
            memory_available_gb: 0.0,
            disk_usage_percent: 0.0,
            disk_free_gb: 0.0,
            running_services_count: 0,
            failed_services: vec![],
            network_interfaces: vec![],
            network_errors: 0,
            disk_health: "unknown".to_string(),
            windows_update_status: "unknown".to_string(),
        })
}
