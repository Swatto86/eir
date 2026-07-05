//! Game Mode's optional power-plan boost — the ONLY system change Game Mode makes.
//!
//! While Game Mode is active *and* the opt-in setting is on, switch to the High
//! Performance power plan; restore the prior plan when it ends. The prior scheme GUID is
//! persisted in the audit DB (`app_state`) so a crash mid-game still restores the plan on
//! the next service startup — a single value, not a batch transaction.

use crate::audit;
use sqlx::SqlitePool;
use std::time::Duration;
use tracing::{info, warn};

/// Well-known High Performance power-scheme GUID (stable across Windows versions;
/// `powercfg /setactive` works even if the plan is hidden in the UI).
const HIGH_PERF_GUID: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
/// `app_state` key holding the scheme GUID to restore to.
const RESTORE_KEY: &str = "power_restore_guid";

/// React to a Game Mode on/off edge. `on = true` (with `power_boost` enabled): save the
/// current scheme and switch to High Performance. `on = false`: restore (regardless of the
/// setting — it may have been turned off mid-game).
pub async fn on_gaming_edge(power_boost: bool, on: bool, db: &SqlitePool) {
    if !on {
        restore(db).await;
        return;
    }
    if !power_boost {
        return;
    }
    match active_scheme_guid().await {
        Some(cur) if cur.eq_ignore_ascii_case(HIGH_PERF_GUID) => {
            // Already on High Performance — nothing to switch or restore.
        }
        Some(cur) => {
            // Record how to restore BEFORE switching; if we can't, don't switch (never
            // change the plan without a recorded way back).
            if let Err(e) = audit::set_state(db, RESTORE_KEY, &cur).await {
                warn!("Game Mode: couldn't persist power-restore GUID, not switching: {e}");
                return;
            }
            set_scheme(HIGH_PERF_GUID).await;
            info!("Game Mode: switched to High Performance power plan (was {cur})");
        }
        None => warn!("Game Mode: couldn't read the active power scheme; not switching"),
    }
}

/// Restore a persisted pre-boost scheme, if any, then clear it. Called on the gaming→off
/// edge AND on service startup — the crash-safe path (a service that died mid-game restores
/// here on next launch).
pub async fn restore(db: &SqlitePool) {
    match audit::get_state(db, RESTORE_KEY).await {
        Ok(Some(guid)) => {
            set_scheme(&guid).await;
            info!("Game Mode: restored power plan to {guid}");
            if let Err(e) = audit::delete_state(db, RESTORE_KEY).await {
                warn!("Game Mode: couldn't clear power-restore GUID: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => warn!("Game Mode: couldn't read power-restore GUID: {e}"),
    }
}

async fn active_scheme_guid() -> Option<String> {
    parse_scheme_guid(&run_powercfg(&["/getactivescheme"]).await?)
}

async fn set_scheme(guid: &str) {
    // `guid` is always our const or a value previously read from powercfg — a real GUID,
    // never user/AI text, so no injection surface into the argv.
    if run_powercfg(&["/setactive", guid]).await.is_none() {
        warn!("Game Mode: powercfg /setactive {guid} did not succeed");
    }
}

/// Run `powercfg.exe` with a 10 s bound, returning stdout on success. Plain exe with a
/// fixed argv (no shell), so nothing to escape.
async fn run_powercfg(args: &[&str]) -> Option<String> {
    let fut = tokio::process::Command::new("powercfg.exe")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(Ok(o)) => {
            warn!("powercfg {args:?} exited {:?}", o.status.code());
            None
        }
        Ok(Err(e)) => {
            warn!("powercfg {args:?} spawn failed: {e}");
            None
        }
        Err(_) => {
            warn!("powercfg {args:?} timed out");
            None
        }
    }
}

/// Extract the scheme GUID from `powercfg /getactivescheme` output, e.g.
/// `Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)`. Locale-robust
/// (matches the GUID shape, not the label text). `None` if no GUID is present.
pub(crate) fn parse_scheme_guid(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find(|tok| is_guid(tok))
        .map(|s| s.to_string())
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&n, p)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_scheme_guid_from_real_output() {
        let out = "\r\nPower Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)\r\n";
        assert_eq!(
            parse_scheme_guid(out).as_deref(),
            Some("381b4222-f694-41f0-9685-ff5bb260df2e")
        );
    }

    #[test]
    fn rejects_output_with_no_guid() {
        assert_eq!(parse_scheme_guid("no guid here (Balanced)"), None);
        // A near-miss (wrong segment lengths) is not accepted.
        assert!(!is_guid("1234-5678"));
        assert!(!is_guid("381b4222f69441f09685ff5bb260df2e"));
        assert!(is_guid("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"));
    }
}
