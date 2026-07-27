use crate::models::{
    DefenderStatus, FirewallStatus, NetworkInterface, SecurityPosture, SystemState,
};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_MORE_DATA};
use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersInfo, IP_ADAPTER_INFO};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, ENUM_SERVICE_STATUS_PROCESSW,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_RUNNING, SERVICE_STATE_ALL,
    SERVICE_STOPPED, SERVICE_WIN32,
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

/// Run a short PowerShell signal probe on the blocking thread with a hard cap.
/// Returns the captured stdout, or None if it failed or exceeded `timeout` (the
/// child is then killed). The snapshot loop awaits each `snapshot_state` before the
/// next tick, so an unbounded probe (e.g. a wedged Get-MpComputerStatus on a
/// degraded box) would otherwise stall every signal — the cap bounds that. Outputs
/// here are tiny (a number / one line), so reading after exit can't deadlock a pipe.
fn ps_capped(command: &str, timeout: std::time::Duration) -> Option<String> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = std::process::Command::new("powershell.exe")
        .args(["-NonInteractive", "-NoProfile", "-Command", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// CPU usage via a WMI PowerShell query, bounded so a slow WMI host can't stall the
/// snapshot loop.
fn get_cpu_usage() -> Option<f32> {
    ps_capped(
        "(Get-WmiObject Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average",
        std::time::Duration::from_secs(15),
    )
    .and_then(|s| s.trim().parse::<f32>().ok())
}

fn get_memory() -> Option<(f32, f32)> {
    let mut mem = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_err() {
        return None;
    }
    let usage = mem.dwMemoryLoad as f32;
    let available_gb = mem.ullAvailPhys as f32 / (1024.0 * 1024.0 * 1024.0);
    Some((usage, available_gb))
}

fn get_disk() -> Option<(f32, f32)> {
    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let path = wide("C:\\");
    if unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(path.as_ptr()),
            None,
            Some(&mut total_bytes),
            Some(&mut free_bytes),
        )
    }
    .is_err()
        || total_bytes == 0
    {
        return None;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let usage = (used as f32 / total_bytes as f32) * 100.0;
    let free_gb = free_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
    Some((usage, free_gb))
}

/// Aggregate SMART health across the machine's physical disks: the worst
/// `HealthStatus` reported by `Get-PhysicalDisk` ("Healthy" | "Warning" |
/// "Unhealthy"), or "unknown" if the query returns nothing. A failed probe is
/// reported and retains the last good value. A disk
/// predicting failure is exactly the early warning a guardian should surface, so this
/// feeds the actionable fingerprint via `SystemState::fault_parts`.
fn get_disk_health() -> Option<String> {
    let out = ps_capped(
        "$h = Get-PhysicalDisk -ErrorAction SilentlyContinue | \
           Select-Object -ExpandProperty HealthStatus; \
         if (-not $h) { 'unknown' } \
         elseif ($h -contains 'Unhealthy') { 'Unhealthy' } \
         elseif ($h -contains 'Warning') { 'Warning' } \
         else { 'Healthy' }",
        std::time::Duration::from_secs(15),
    );
    out.map(|s| {
        let t = s.trim();
        if t.is_empty() {
            "unknown".to_string()
        } else {
            t.to_string()
        }
    })
}

/// Total NIC packet errors (received + outbound) across adapters, via the standard
/// CIM statistics class. A missing property or failed query is reported so it cannot
/// masquerade as a healthy zero.
fn get_network_errors() -> Option<u32> {
    let out = ps_capped(
        "try { \
           $s = Get-CimInstance -Namespace root/StandardCimv2 \
                -ClassName MSFT_NetAdapterStatisticsSettingData -ErrorAction Stop; \
           $e = 0; \
           foreach ($a in $s) { \
             $e += [int64]$a.ReceivedPacketErrors + [int64]$a.OutboundPacketErrors } \
           $e \
         } catch { exit 1 }",
        std::time::Duration::from_secs(15),
    );
    out.and_then(|s| s.trim().parse::<i64>().ok())
        .and_then(|n| u32::try_from(n.clamp(0, i64::from(u32::MAX))).ok())
}

/// Enumerate services and return (running_count, failed_names).
///
/// A "failed" service is one that is fully STOPPED with an *abnormal* exit code.
/// The previous version enumerated only `SERVICE_ACTIVE` and flagged anything not
/// RUNNING, which (a) missed the crashed-to-STOPPED services that matter most and
/// (b) mislabelled transient PENDING/PAUSED states as failures. Exit code 0 (clean
/// stop) and 1077 (`ERROR_SERVICE_NEVER_STARTED`, the normal state of Manual/Disabled
/// services) are excluded so `failed_services` isn't flooded with services that are
/// stopped by design.
fn get_services() -> Option<(usize, Vec<String>)> {
    const ERROR_SERVICE_NEVER_STARTED: u32 = 1077;
    let manager = match unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE)
    } {
        Ok(h) => h,
        Err(_) => return None,
    };

    let mut running = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut resume_handle: u32 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    let mut complete = false;

    // Bounded loop over resume batches (guards against a pathological non-advancing
    // resume handle); SERVICE_STATE_ALL can span hundreds of services.
    for _ in 0..64 {
        let mut bytes_needed: u32 = 0;
        let mut services_returned: u32 = 0;
        let result = unsafe {
            EnumServicesStatusExW(
                manager,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                Some(&mut buf),
                &mut bytes_needed,
                &mut services_returned,
                Some(&mut resume_handle),
                PCWSTR::null(),
            )
        };

        if services_returned == 0 {
            // Buffer too small for even one record — grow and retry; else nothing left.
            if bytes_needed as usize > buf.len() {
                buf.resize(bytes_needed as usize, 0);
                continue;
            }
            complete = result.is_ok();
            break;
        }

        let records = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
                services_returned as usize,
            )
        };
        for svc in records {
            let ssp = &svc.ServiceStatusProcess;
            if ssp.dwCurrentState == SERVICE_RUNNING {
                running += 1;
                continue;
            }
            if ssp.dwCurrentState == SERVICE_STOPPED
                && ssp.dwWin32ExitCode != 0
                && ssp.dwWin32ExitCode != ERROR_SERVICE_NEVER_STARTED
            {
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

        // Ok => all remaining services fit in this batch; ERROR_MORE_DATA => another
        // batch remains (resume_handle advanced); any other error => stop.
        match result {
            Ok(()) => {
                complete = true;
                break;
            }
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => continue,
            Err(_) => break,
        }
    }

    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    complete.then_some((running, failed))
}

fn i8_array_to_string(arr: &[i8]) -> String {
    let bytes: Vec<u8> = arr.iter().map(|&b| b as u8).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn get_network_interfaces() -> Option<Vec<NetworkInterface>> {
    let mut interfaces = Vec::new();
    let mut buf_size: u32 = 16384;
    let mut buf = vec![0u8; buf_size as usize];

    let mut result = unsafe {
        GetAdaptersInfo(
            Some(buf.as_mut_ptr() as *mut IP_ADAPTER_INFO),
            &mut buf_size,
        )
    };

    // On a machine with many adapters (VPN/Hyper-V/Docker) 16 KiB is too small;
    // GetAdaptersInfo returns ERROR_BUFFER_OVERFLOW and writes the needed size into
    // buf_size. Grow and retry once instead of reporting zero interfaces.
    if result == ERROR_BUFFER_OVERFLOW.0 {
        buf = vec![0u8; buf_size as usize];
        result = unsafe {
            GetAdaptersInfo(
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_INFO),
                &mut buf_size,
            )
        };
    }

    if result != 0 {
        return None;
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

    Some(interfaces)
}

fn get_windows_update_status() -> Option<String> {
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
            return None;
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
            return None;
        }

        let chars: &[u16] = std::slice::from_raw_parts(
            data.as_ptr() as *const u16,
            (data_len as usize / 2).saturating_sub(1),
        );
        Some(format!("last_install: {}", String::from_utf16_lossy(chars)))
    }
}

/// Read a REG_DWORD value, returning None if the key/value is missing or is not a
/// 4-byte REG_DWORD (the type is checked, not just the length, so a 4-byte string or
/// binary value is rejected rather than misread as a u32).
fn read_reg_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let subkey_w = wide(subkey);
    let value_w = wide(value);
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(root, PCWSTR(subkey_w.as_ptr()), 0, KEY_READ, &mut hkey).is_err() {
            return None;
        }
        let mut data: u32 = 0;
        let mut data_len = std::mem::size_of::<u32>() as u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let ok = RegQueryValueExW(
            hkey,
            PCWSTR(value_w.as_ptr()),
            None,
            Some(&mut value_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_len),
        )
        .is_ok();
        let _ = RegCloseKey(hkey);
        if ok && value_type == REG_DWORD && data_len as usize == std::mem::size_of::<u32>() {
            Some(data)
        } else {
            None
        }
    }
}

/// Resolve a profile's *effective* firewall state from the Group Policy value (if
/// any) and the local SharedAccess value. Group Policy wins when present:
///   - policy ON  → Some(true): the firewall is enforced on, nothing to do.
///   - policy OFF → None: a GPO is deliberately holding it off and `netsh` cannot
///     override that, so Eir treats it as "not ours to fix" rather than a fault —
///     this is what stops a futile firewall_enable loop on managed machines.
///   - no policy  → the local value is the effective one (and locally fixable).
fn effective_firewall(policy: Option<bool>, local: Option<bool>) -> Option<bool> {
    match policy {
        Some(true) => Some(true),
        Some(false) => None,
        None => local,
    }
}

/// Firewall on/off per profile, reflecting the effective (GPO-aware) state. A value
/// that cannot be read stays None (unknown), so the AI never reads "couldn't read it"
/// as "firewall is off". Note the SharedAccess store calls the private profile
/// "StandardProfile" while the GPO store calls it "PrivateProfile".
fn get_firewall() -> FirewallStatus {
    const LOCAL: &str =
        "SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy";
    const POLICY: &str = "SOFTWARE\\Policies\\Microsoft\\WindowsFirewall";
    let resolve = |local_dir: &str, policy_dir: &str| {
        let read = |base: &str, dir: &str| {
            read_reg_dword(
                HKEY_LOCAL_MACHINE,
                &format!("{base}\\{dir}"),
                "EnableFirewall",
            )
            .map(|v| v != 0)
        };
        effective_firewall(read(POLICY, policy_dir), read(LOCAL, local_dir))
    };
    FirewallStatus {
        domain: resolve("DomainProfile", "DomainProfile"),
        private: resolve("StandardProfile", "PrivateProfile"),
        public: resolve("PublicProfile", "PublicProfile"),
    }
}

/// Parse the pipe-delimited line emitted by the Get-MpComputerStatus query:
/// "<realtime>|<antivirus>|<signature_age_days>". Any field may be empty/garbage,
/// in which case it parses to None rather than failing the whole snapshot.
fn parse_defender_status(line: &str) -> DefenderStatus {
    let mut parts = line.trim().split('|');
    let parse_bool = |s: Option<&str>| match s.map(|x| x.trim().to_lowercase()) {
        Some(v) if v == "true" => Some(true),
        Some(v) if v == "false" => Some(false),
        _ => None,
    };
    let realtime_enabled = parse_bool(parts.next());
    let antivirus_enabled = parse_bool(parts.next());
    let signature_age_days = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
    DefenderStatus {
        realtime_enabled,
        antivirus_enabled,
        signature_age_days,
    }
}

/// Query Windows Defender via PowerShell, bounded by ps_capped. Absent Defender, a
/// failed query, or a timeout leaves every field None (handled by
/// parse_defender_status on empty output).
fn get_defender() -> Option<DefenderStatus> {
    let status = ps_capped(
        "$s = Get-MpComputerStatus -ErrorAction SilentlyContinue; \
         if ($s) { '{0}|{1}|{2}' -f $s.RealTimeProtectionEnabled, $s.AntivirusEnabled, $s.AntivirusSignatureAge } \
         else { exit 1 }",
        std::time::Duration::from_secs(15),
    )
    .map(|s| parse_defender_status(&s))?;
    (status.realtime_enabled.is_some()
        || status.antivirus_enabled.is_some()
        || status.signature_age_days.is_some())
    .then_some(status)
}

fn retain_or_report<T: Clone>(
    value: Option<T>,
    previous: &T,
    source: &'static str,
    errors: &mut Vec<String>,
) -> T {
    value.unwrap_or_else(|| {
        errors.push(source.to_string());
        previous.clone()
    })
}

fn snapshot_state(previous: SystemState) -> SystemState {
    let mut collector_errors = Vec::new();
    let uptime_secs = get_uptime_secs();
    let cpu_usage_percent = retain_or_report(
        get_cpu_usage(),
        &previous.cpu_usage_percent,
        "cpu",
        &mut collector_errors,
    );
    let (memory_usage_percent, memory_available_gb) = retain_or_report(
        get_memory(),
        &(previous.memory_usage_percent, previous.memory_available_gb),
        "memory",
        &mut collector_errors,
    );
    let (disk_usage_percent, disk_free_gb) = retain_or_report(
        get_disk(),
        &(previous.disk_usage_percent, previous.disk_free_gb),
        "disk",
        &mut collector_errors,
    );
    let (running_services_count, failed_services) = retain_or_report(
        get_services(),
        &(
            previous.running_services_count,
            previous.failed_services.clone(),
        ),
        "services",
        &mut collector_errors,
    );
    let network_interfaces = retain_or_report(
        get_network_interfaces(),
        &previous.network_interfaces,
        "network_interfaces",
        &mut collector_errors,
    );
    let network_errors = retain_or_report(
        get_network_errors(),
        &previous.network_errors,
        "network_errors",
        &mut collector_errors,
    );
    let disk_health = retain_or_report(
        get_disk_health(),
        &previous.disk_health,
        "disk_health",
        &mut collector_errors,
    );
    let windows_update_status = retain_or_report(
        get_windows_update_status(),
        &previous.windows_update_status,
        "windows_update",
        &mut collector_errors,
    );
    let firewall_probe = get_firewall();
    let firewall_known = firewall_probe.domain.is_some()
        || firewall_probe.private.is_some()
        || firewall_probe.public.is_some();
    let firewall = retain_or_report(
        firewall_known.then_some(firewall_probe),
        &previous.security.firewall,
        "firewall",
        &mut collector_errors,
    );
    let defender = retain_or_report(
        get_defender(),
        &previous.security.defender,
        "defender",
        &mut collector_errors,
    );
    let security = SecurityPosture { firewall, defender };

    SystemState {
        collected_at: chrono::Utc::now().timestamp(),
        collector_errors,
        uptime_secs,
        cpu_usage_percent,
        memory_usage_percent,
        memory_available_gb,
        disk_usage_percent,
        disk_free_gb,
        running_services_count,
        failed_services,
        network_interfaces,
        network_errors,
        disk_health,
        windows_update_status,
        security,
    }
}

/// Compact key of a snapshot's actionable faults (shared definition:
/// [`SystemState::fault_parts`]). Used to wake the decision loop only when the
/// fault state *changes* — a persistent fault must not re-trigger a reaction on
/// every poll.
fn fault_key(s: &SystemState) -> String {
    let mut parts = s.fault_parts();
    parts.sort();
    parts.join("\n")
}

/// Whether the fault set changed between two consecutive poll snapshots (keyed by
/// `fault_key`). Any change wakes the decision loop — a new/changed fault AND a fault
/// clearing (`cur` empty), so a recovery is reflected promptly — while an unchanged key
/// (including healthy → healthy) stays quiet so a persistent fault doesn't re-trigger
/// every poll.
fn fault_changed(prev: &str, cur: &str) -> bool {
    prev != cur
}

/// Force an immediate services-only rescan for the manual "Refresh status" command.
/// Fast (SCM enumeration, no PowerShell), so it can be awaited inline in the command
/// handler without stalling the loop; returns the fresh failed-services set. cpu/mem/disk
/// still refresh on the normal cadence — the manual refresh targets the stale-service
/// complaint, and a full `snapshot_state` would shell out to PowerShell (up to 15 s).
///
/// Also writes the result into the shared cache `wmi::current` reads. Without this, the
/// next decision tick (or any reactive wake) would read the still-stale cached snapshot —
/// the background poller only re-scans every few minutes — and overwrite `st.failed_services`
/// back to the stale value, silently re-showing the service the user just cleared.
pub async fn rescan_failed_services(shared: &SharedState) -> Vec<String> {
    let result = tokio::task::spawn_blocking(get_services)
        .await
        .ok()
        .flatten();
    if let Some((running, failed)) = result {
        if let Ok(mut guard) = shared.lock() {
            // Only correct the services field; leave cpu/mem/disk to the next full poll.
            if let Some(s) = guard.as_mut() {
                s.running_services_count = running;
                s.failed_services = failed.clone();
                s.collector_errors.retain(|source| source != "services");
            }
        }
        failed
    } else {
        warn!("manual service rescan failed; retaining the last good service state");
        if let Ok(mut guard) = shared.lock() {
            if let Some(s) = guard.as_mut() {
                if !s.collector_errors.iter().any(|source| source == "services") {
                    s.collector_errors.push("services".to_string());
                }
                return s.failed_services.clone();
            }
        }
        Vec::new()
    }
}

pub fn spawn(
    poll_interval_secs: u64,
    trigger: super::TriggerTx,
) -> (SharedState, watch::Sender<()>) {
    let shared: SharedState = Arc::new(Mutex::new(None));
    let shared_clone = shared.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
        let mut last_fault_key = String::new();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let previous = current(&shared_clone);
                    match tokio::task::spawn_blocking(move || snapshot_state(previous)).await {
                        Ok(s) => {
                            info!(
                                cpu = s.cpu_usage_percent,
                                mem = s.memory_usage_percent,
                                disk_free_gb = s.disk_free_gb,
                                failed_services = s.failed_services.len(),
                                update = %s.windows_update_status,
                                collector_errors = ?s.collector_errors,
                                "WMI snapshot"
                            );
                            // Wake the decision loop when a NEW fault appears
                            // (a changed, non-empty fault set).
                            let key = fault_key(&s);
                            // Wake the loop on ANY change to the fault set — including a
                            // fault CLEARING (key → empty) — so a recovered service or
                            // re-enabled firewall is reflected within one reactive debounce
                            // instead of waiting for the next scheduled decision tick. A
                            // pure recovery makes `actionable_fingerprint` empty, so the
                            // idle-skip gate suppresses the AI call: this refreshes the UI
                            // without spending on analysis.
                            let changed = fault_changed(&last_fault_key, &key);
                            last_fault_key = key;
                            if let Ok(mut guard) = shared_clone.lock() {
                                *guard = Some(s);
                            }
                            if changed {
                                let _ = trigger.try_send(());
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
            collected_at: 0,
            collector_errors: vec!["not_collected".to_string()],
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
            security: SecurityPosture::default(),
        })
}

#[cfg(test)]
mod tests {
    use super::{effective_firewall, fault_changed, parse_defender_status, retain_or_report};

    #[test]
    fn failed_probe_keeps_last_good_value_and_reports_the_source() {
        let mut errors = Vec::new();
        assert_eq!(retain_or_report(None, &42_u32, "network", &mut errors), 42);
        assert_eq!(errors, ["network"]);
    }

    #[tokio::test]
    async fn rescan_overwrites_the_cached_failed_services() {
        // Seed the shared cache with a stale snapshot, then rescan: the cache must be
        // overwritten with the fresh result (so a later wmi::current() read can't
        // re-populate st.failed_services with the stale value), and the returned vec must
        // match the cache. We don't assert the *contents* (machine-dependent), only that
        // the stale sentinel is gone and cache == return.
        use crate::models::SystemState;
        let shared: super::SharedState =
            std::sync::Arc::new(std::sync::Mutex::new(Some(SystemState {
                failed_services: vec!["StaleSentinelSvc".into()],
                ..Default::default()
            })));
        let returned = super::rescan_failed_services(&shared).await;
        let cached = shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .failed_services
            .clone();
        assert_eq!(cached, returned, "cache must equal the rescan result");
        assert!(
            !cached.iter().any(|s| s == "StaleSentinelSvc"),
            "stale value must be overwritten, not retained"
        );
    }

    #[test]
    fn fault_change_wakes_on_appear_clear_and_change_but_not_on_steady_state() {
        // Appear (healthy → fault) and clear (fault → healthy) must BOTH wake the loop —
        // the clear case is the fix (a recovered service was previously never re-pushed).
        assert!(fault_changed("", "S|Spooler"), "fault appearing must wake");
        assert!(fault_changed("S|Spooler", ""), "fault clearing must wake");
        // A different fault set is a change too.
        assert!(fault_changed("S|Spooler", "S|W32Time"));
        // Steady state (unchanged) must stay quiet so a persistent fault doesn't
        // re-trigger every poll — including healthy → healthy.
        assert!(!fault_changed("S|Spooler", "S|Spooler"));
        assert!(!fault_changed("", ""));
    }

    #[test]
    fn gpo_enforced_on_firewall_is_never_a_fault() {
        // GPO forces the firewall ON; the stale local value must not surface a fault.
        assert_eq!(effective_firewall(Some(true), Some(false)), Some(true));
    }

    #[test]
    fn gpo_enforced_off_firewall_is_hands_off() {
        // A GPO holding it off is not Eir's to fix (netsh can't override) → None.
        assert_eq!(effective_firewall(Some(false), Some(true)), None);
    }

    #[test]
    fn locally_managed_firewall_uses_the_local_value() {
        assert_eq!(effective_firewall(None, Some(false)), Some(false));
        assert_eq!(effective_firewall(None, Some(true)), Some(true));
        assert_eq!(effective_firewall(None, None), None);
    }

    #[test]
    fn defender_status_parses_a_healthy_line() {
        let s = parse_defender_status("True|True|0\r\n");
        assert_eq!(s.realtime_enabled, Some(true));
        assert_eq!(s.antivirus_enabled, Some(true));
        assert_eq!(s.signature_age_days, Some(0));
    }

    #[test]
    fn defender_status_flags_disabled_realtime_and_stale_signatures() {
        let s = parse_defender_status("False|True|14");
        assert_eq!(s.realtime_enabled, Some(false));
        assert_eq!(s.signature_age_days, Some(14));
    }

    #[test]
    fn defender_status_empty_output_is_all_unknown() {
        let s = parse_defender_status("");
        assert_eq!(s.realtime_enabled, None);
        assert_eq!(s.antivirus_enabled, None);
        assert_eq!(s.signature_age_days, None);
    }
}
