//! System-file repair actions: `sfc /scannow` and `DISM /Online /Cleanup-Image
//! /RestoreHealth`. Both run as SYSTEM (the service is already elevated, no UAC), are
//! long-running (SFC is typically several minutes; DISM can be much longer and may
//! pull replacement files from Windows Update), and only ever run after explicit user
//! approval — they are never on the auto-execute whitelist.

use anyhow::Result;
use std::time::Duration;

/// Ceiling for a repair command. Deliberately generous: SFC can exceed 10 minutes and
/// DISM's online restore can run far longer on a slow link. The executor worker gives
/// these actions a matching job timeout (see `main.rs` `exec_timeout`).
pub const REPAIR_TIMEOUT: Duration = Duration::from_secs(40 * 60);

/// Verify and repair protected Windows system files with `sfc /scannow`.
pub async fn sfc_scan() -> Result<String> {
    let script = "$out = sfc /scannow 2>&1 | Out-String; \
                  Write-Output ($out.Trim()); \
                  if ($LASTEXITCODE -ne 0) { throw \"sfc exited with $LASTEXITCODE\" }";
    let out = super::powershell::run_diagnostic_with_timeout(script, REPAIR_TIMEOUT).await?;
    Ok(summarise("SFC", &out))
}

/// Repair the Windows component store with DISM's online RestoreHealth.
pub async fn dism_restore_health() -> Result<String> {
    let script = "$out = DISM /Online /Cleanup-Image /RestoreHealth 2>&1 | Out-String; \
                  Write-Output ($out.Trim()); \
                  if ($LASTEXITCODE -ne 0) { throw \"DISM exited with $LASTEXITCODE\" }";
    let out = super::powershell::run_diagnostic_with_timeout(script, REPAIR_TIMEOUT).await?;
    Ok(summarise("DISM", &out))
}

/// These tools emit pages of progress; keep the last few meaningful lines (where the
/// outcome — "did not find any integrity violations", "successfully repaired", etc. —
/// lives), capped for the activity feed.
fn summarise(tool: &str, raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.chars().all(|c| c == '=' || c == '.'))
        .collect();
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    let body = if tail.chars().count() > 300 {
        tail.chars().take(299).chain(['…']).collect()
    } else {
        tail
    };
    if body.is_empty() {
        format!("{tool} completed")
    } else {
        format!("{tool}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarise_keeps_tail_and_caps() {
        let raw = "Beginning system scan.\n\
                   ======================\n\
                   Verification 100% complete.\n\
                   Windows Resource Protection did not find any integrity violations.";
        let s = summarise("SFC", raw);
        assert!(s.starts_with("SFC:"));
        assert!(s.contains("did not find any integrity violations"));
        // The banner line of '=' is dropped.
        assert!(!s.contains("======"));
    }

    #[test]
    fn summarise_handles_empty() {
        assert_eq!(summarise("DISM", "\n\n"), "DISM completed");
    }
}
