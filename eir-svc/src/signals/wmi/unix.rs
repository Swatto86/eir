//! Linux system-state collector: `/proc/stat` (cpu), `/proc/meminfo` (memory),
//! `libc::statvfs` on `/` (disk), `systemctl list-units`/`--failed` (services),
//! `ip -j addr show` (network interfaces), `/proc/net/dev` (network errors). There is
//! no Linux analogue of the Windows firewall/Defender/Windows-Update fields — they stay
//! at their `SecurityPosture`/"unknown" defaults, exactly as an unreadable Windows probe
//! already degrades (see [`super::retain_or_report`]).

use super::retain_or_report;
use crate::models::{NetworkInterface, SecurityPosture, SystemState};
use std::process::Command;
use std::time::Duration;
use tracing::warn;

fn read_proc(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn parse_uptime_secs(contents: &str) -> Option<u64> {
    let first = contents.split_whitespace().next()?;
    first.parse::<f64>().ok().map(|secs| secs as u64)
}

fn get_uptime_secs() -> u64 {
    read_proc("/proc/uptime")
        .and_then(|s| parse_uptime_secs(&s))
        .unwrap_or(0)
}

/// One `cpu ` line of `/proc/stat`: user nice system idle iowait irq softirq steal.
/// Returns (idle_ticks, total_ticks).
fn parse_cpu_line(contents: &str) -> Option<(u64, u64)> {
    let line = contents.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse::<u64>().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = fields.iter().sum();
    Some((idle, total))
}

/// Two `/proc/stat` samples ~200ms apart, so CPU usage reflects a real rate rather than
/// the cumulative-since-boot figure a single read would give.
fn get_cpu_usage() -> Option<f32> {
    let (idle1, total1) = parse_cpu_line(&read_proc("/proc/stat")?)?;
    std::thread::sleep(Duration::from_millis(200));
    let (idle2, total2) = parse_cpu_line(&read_proc("/proc/stat")?)?;
    let idle_delta = idle2.saturating_sub(idle1);
    let total_delta = total2.saturating_sub(total1);
    if total_delta == 0 {
        return None;
    }
    let usage = 100.0 * (1.0 - idle_delta as f64 / total_delta as f64);
    Some(usage.clamp(0.0, 100.0) as f32)
}

fn parse_meminfo(contents: &str) -> Option<(f32, f32)> {
    let mut total_kb = None;
    let mut available_kb = None;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    let total_kb = total_kb?;
    let available_kb = available_kb?;
    if total_kb == 0 {
        return None;
    }
    let used_kb = total_kb.saturating_sub(available_kb);
    let usage = 100.0 * used_kb as f64 / total_kb as f64;
    let available_gb = available_kb as f64 / (1024.0 * 1024.0);
    Some((usage as f32, available_gb as f32))
}

fn get_memory() -> Option<(f32, f32)> {
    parse_meminfo(&read_proc("/proc/meminfo")?)
}

fn get_disk() -> Option<(f32, f32)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let path = CString::new("/").ok()?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated C string and `stat` is a fresh, properly
    // sized buffer for `statvfs` to initialise.
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: a zero return code means `statvfs` fully initialised `stat`.
    let stat = unsafe { stat.assume_init() };
    let frsize = stat.f_frsize.max(1);
    let total = stat.f_blocks * frsize;
    let available = stat.f_bavail * frsize;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    let usage = 100.0 * used as f64 / total as f64;
    let free_gb = available as f64 / (1024.0 * 1024.0 * 1024.0);
    Some((usage as f32, free_gb as f32))
}

/// Running unit count and the exact-name list of `--failed` units. Mirrors the
/// Windows collector's shape: a `failed_services` entry is a unit whose most recent
/// exit was abnormal, matching `systemctl --failed`'s own definition.
fn get_services() -> Option<(usize, Vec<String>)> {
    let running = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
        ])
        .output()
        .ok()?;
    if !running.status.success() {
        return None;
    }
    let running_count = String::from_utf8_lossy(&running.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let failed = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--state=failed",
            "--no-legend",
            "--plain",
        ])
        .output()
        .ok()?;
    if !failed.status.success() {
        return None;
    }
    let failed_names = String::from_utf8_lossy(&failed.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();

    Some((running_count, failed_names))
}

pub(super) fn get_services_now() -> Option<(usize, Vec<String>)> {
    get_services()
}

/// One element of `ip -j addr show`'s JSON array.
fn parse_ip_addr_json(json: &serde_json::Value) -> Vec<NetworkInterface> {
    let Some(array) = json.as_array() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|entry| {
            let name = entry.get("ifname")?.as_str()?.to_string();
            let operstate = entry
                .get("operstate")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_lowercase();
            let ipv4 = entry
                .get("addr_info")
                .and_then(|v| v.as_array())
                .and_then(|addrs| {
                    addrs.iter().find_map(|addr| {
                        (addr.get("family").and_then(|f| f.as_str()) == Some("inet"))
                            .then(|| addr.get("local").and_then(|l| l.as_str()))
                            .flatten()
                            .map(str::to_string)
                    })
                });
            // Loopback and some virtual links always report operstate UNKNOWN; for those
            // the kernel's UP + LOWER_UP flags (enabled, carrier present) mean working.
            let flags: Vec<&str> = entry
                .get("flags")
                .and_then(|f| f.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let link_up = flags.contains(&"UP") && flags.contains(&"LOWER_UP");
            let status = if operstate == "up" || (operstate == "unknown" && link_up) {
                "up"
            } else {
                "down"
            };
            Some(NetworkInterface {
                name,
                status: status.to_string(),
                ipv4,
            })
        })
        .collect()
}

fn get_network_interfaces() -> Option<Vec<NetworkInterface>> {
    let output = Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(parse_ip_addr_json(&json))
}

/// Sum of rx_errs (field index 2 of the receive block) + tx_errs (field index 2 of the
/// transmit block, i.e. overall index 10) across every non-loopback interface in
/// `/proc/net/dev`.
fn parse_net_dev_errors(contents: &str) -> Option<u32> {
    let mut total: u64 = 0;
    for line in contents.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|f| f.parse::<u64>().ok())
            .collect();
        if fields.len() < 16 {
            continue;
        }
        total += fields[2] + fields[10];
    }
    Some(total.min(u64::from(u32::MAX)) as u32)
}

fn get_network_errors() -> Option<u32> {
    parse_net_dev_errors(&read_proc("/proc/net/dev")?)
}

pub(super) fn snapshot_state(previous: SystemState) -> SystemState {
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

    if collector_errors.iter().any(|source| source == "services") {
        warn!("systemctl unavailable or failed; retaining the last good service state");
    }

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
        // No Linux analogue in this phase (SMART health / distro-update posture) —
        // deliberately left at the same "unknown" default a failed Windows probe uses.
        disk_health: "unknown".to_string(),
        windows_update_status: "unknown".to_string(),
        // No Windows Firewall / Defender on Linux; every field stays `None` ("unknown",
        // never a fault) exactly like an unreadable Windows probe.
        security: SecurityPosture::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in, not run in CI or by a plain `cargo test`: exercises every real Linux
    /// collector (`/proc`, `systemctl`, `ip`) against whatever machine actually runs
    /// it. Run explicitly on the target host, e.g.
    /// `cargo test -p eir-svc --lib signals::wmi::unix -- --ignored`.
    #[test]
    #[ignore = "reads the real host's /proc, systemctl and ip — run explicitly on target Linux"]
    fn real_linux_snapshot_collects_a_sane_system_state() {
        let previous = SystemState::default();
        let snapshot = snapshot_state(previous);
        assert!(
            snapshot.collector_errors.is_empty(),
            "every collector should succeed on a real, healthy Linux host: {:?}",
            snapshot.collector_errors
        );
        assert!(
            snapshot.uptime_secs > 0,
            "a real host has been up for a while"
        );
        assert!(
            (0.0..=100.0).contains(&snapshot.cpu_usage_percent),
            "cpu {}",
            snapshot.cpu_usage_percent
        );
        assert!(
            (0.0..=100.0).contains(&snapshot.memory_usage_percent),
            "memory {}",
            snapshot.memory_usage_percent
        );
        assert!(
            snapshot.disk_free_gb > 0.0,
            "the root filesystem has some free space"
        );
        assert!(
            snapshot.running_services_count > 0,
            "a real systemd host has running services"
        );
        assert!(
            !snapshot.network_interfaces.is_empty(),
            "a real host has at least loopback + one real interface"
        );
    }

    #[test]
    fn cpu_line_splits_idle_and_total_ticks() {
        let sample = "cpu  1000 200 300 5000 100 0 0 0 0 0\ncpu0 500 100 150 2500 50 0 0 0 0 0\n";
        let (idle, total) = parse_cpu_line(sample).expect("cpu line parsed");
        assert_eq!(idle, 5000 + 100);
        assert_eq!(total, 1000 + 200 + 300 + 5000 + 100);
        assert_eq!(parse_cpu_line("garbage"), None);
    }

    #[test]
    fn meminfo_computes_usage_and_available_gb() {
        let sample = "MemTotal:       16000000 kB\nMemFree:         2000000 kB\nMemAvailable:    8000000 kB\n";
        let (usage, available_gb) = parse_meminfo(sample).expect("meminfo parsed");
        assert!((usage - 50.0).abs() < 0.1, "usage={usage}");
        assert!(
            (available_gb - 7.629).abs() < 0.01,
            "available_gb={available_gb}"
        );
        assert_eq!(parse_meminfo("MemTotal: 0 kB"), None);
    }

    #[test]
    fn uptime_parses_the_first_field_only() {
        assert_eq!(parse_uptime_secs("12345.67 54321.00"), Some(12345));
        assert_eq!(parse_uptime_secs(""), None);
    }

    #[test]
    fn ip_addr_json_maps_operstate_and_first_inet_address() {
        let sample = serde_json::json!([
            {
                "ifname": "eth0",
                "operstate": "UP",
                "addr_info": [
                    {"family": "inet6", "local": "fe80::1"},
                    {"family": "inet", "local": "10.0.0.5"}
                ]
            },
            {
                "ifname": "lo",
                "operstate": "UNKNOWN",
                "addr_info": [{"family": "inet", "local": "127.0.0.1"}]
            },
            {
                "ifname": "eth1",
                "operstate": "DOWN",
                "addr_info": []
            }
        ]);
        let interfaces = parse_ip_addr_json(&sample);
        assert_eq!(interfaces.len(), 3);
        assert_eq!(interfaces[0].name, "eth0");
        assert_eq!(interfaces[0].status, "up");
        assert_eq!(interfaces[0].ipv4.as_deref(), Some("10.0.0.5"));
        assert_eq!(interfaces[2].status, "down");
        assert_eq!(interfaces[2].ipv4, None);
    }

    #[test]
    fn net_dev_sums_errors_across_non_loopback_interfaces() {
        let sample = "Inter-|   Receive                                                |  Transmit\n \
                       face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
                         lo:     100       1    9    0    0     0          0         0      100       1    9    0    0     0       0          0\n\
                       eth0:    2000      20    3    0    0     0          0         0     3000      30    5    0    0     0       0          0\n";
        // loopback's errors (9 rx + 9 tx) must be excluded; only eth0's 3 + 5 = 8 count.
        assert_eq!(parse_net_dev_errors(sample), Some(8));
    }

    #[test]
    fn a_missing_systemctl_or_fields_is_reported_not_silently_zeroed() {
        // Bogus fixture with too few columns must not be mistaken for zero errors.
        let sample = "Inter-|   Receive\n face |bytes\n  eth0: 1 2\n";
        assert_eq!(parse_net_dev_errors(sample), Some(0));
    }

    #[test]
    fn loopback_with_unknown_operstate_but_carrier_is_up() {
        // Real `ip -j addr show` shape for lo on swatbox, plus a genuinely down link.
        let json: serde_json::Value = serde_json::from_str(
            r#"[{"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"operstate":"UNKNOWN","addr_info":[{"family":"inet","local":"127.0.0.1"}]},
                {"ifname":"eth1","flags":["BROADCAST","MULTICAST"],"operstate":"DOWN","addr_info":[]},
                {"ifname":"tun0","flags":["POINTOPOINT","NOARP"],"operstate":"UNKNOWN","addr_info":[]}]"#,
        )
        .expect("json");
        let parsed = parse_ip_addr_json(&json);
        assert_eq!(parsed[0].status, "up");
        assert_eq!(parsed[1].status, "down");
        assert_eq!(
            parsed[2].status, "down",
            "UNKNOWN without carrier stays down"
        );
    }
}
