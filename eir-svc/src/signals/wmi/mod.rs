//! Periodic system-state snapshot: cpu/mem/disk, running/failed services, network
//! interfaces, and (Windows only) firewall/Defender/Windows-Update posture. The
//! collector body is platform-specific (`windows.rs` / `unix.rs`); everything here —
//! the shared cache, the fault-change wake, the manual rescan, and `spawn`/`current` —
//! is OS-neutral and unchanged from before the Linux port.

use crate::models::SystemState;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::{get_services_now, snapshot_state};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::{get_services_now, snapshot_state};

pub type SharedState = Arc<Mutex<Option<SystemState>>>;

/// Keep the last good value and record which collector failed, rather than letting a
/// single flaky probe blank out an otherwise-good snapshot.
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
/// Fast (no full snapshot), so it can be awaited inline in the command handler without
/// stalling the loop; returns the fresh failed-services set. cpu/mem/disk still refresh
/// on the normal cadence — the manual refresh targets the stale-service complaint, and
/// a full `snapshot_state` is comparatively expensive (shells out on Windows, forks
/// `systemctl` on Linux).
///
/// Also writes the result into the shared cache `wmi::current` reads. Without this, the
/// next decision tick (or any reactive wake) would read the still-stale cached snapshot —
/// the background poller only re-scans every few minutes — and overwrite `st.failed_services`
/// back to the stale value, silently re-showing the service the user just cleared.
pub async fn rescan_failed_services(shared: &SharedState) -> Vec<String> {
    let result = tokio::task::spawn_blocking(get_services_now)
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
                                collector_errors = ?s.collector_errors,
                                "System-state snapshot"
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
                        Err(e) => warn!("System-state snapshot task panicked: {e}"),
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
            security: crate::models::SecurityPosture::default(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_probe_keeps_last_good_value_and_reports_the_source() {
        let mut errors = Vec::new();
        assert_eq!(retain_or_report(None, &42_u32, "network", &mut errors), 42);
        assert_eq!(errors, ["network"]);
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

    #[tokio::test]
    async fn rescan_overwrites_the_cached_failed_services() {
        // Seed the shared cache with a stale snapshot, then rescan: the cache must be
        // overwritten with the fresh result (so a later wmi::current() read can't
        // re-populate st.failed_services with the stale value), and the returned vec must
        // match the cache. We don't assert the *contents* (machine-dependent), only that
        // the stale sentinel is gone and cache == return.
        let shared: SharedState = Arc::new(Mutex::new(Some(SystemState {
            failed_services: vec!["StaleSentinelSvc".into()],
            ..Default::default()
        })));
        let returned = rescan_failed_services(&shared).await;
        let cached = shared
            .lock()
            .map(|g| g.as_ref().map(|s| s.failed_services.clone()))
            .ok()
            .flatten()
            .unwrap_or_default();
        assert_eq!(cached, returned, "cache must equal the rescan result");
        assert!(
            !cached.iter().any(|s| s == "StaleSentinelSvc"),
            "stale value must be overwritten, not retained"
        );
    }
}
