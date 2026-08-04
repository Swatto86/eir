use anyhow::{bail, Result};

const PROTECTED_PROCESSES: &[&str] = &[
    "System", "Idle", "lsass", "winlogon", "csrss", "smss", "wininit", "services", "svchost",
    "ntoskrnl", "eir-svc", "MsMpEng",
];

pub async fn kill(process_name: &str) -> Result<String> {
    validate_kill_target(process_name)?;
    // A Windows process's Name has no extension, so Get-Process -Name never matches a
    // ".exe"-suffixed string. Strip it (validate_kill_target strips it for the blocklist)
    // before building the query.
    let safe_name = process_query_name(process_name).replace('\'', "''");
    // Report success only if a process actually existed and was stopped. The old script
    // used `-ErrorAction SilentlyContinue` and unconditionally wrote success, so a typo'd/
    // absent/hallucinated name or an access-denied kill was logged as a fabricated success
    // (and suppressed retries via safety::rate_limited). Now: throw if nothing matches, and
    // stop with `-ErrorAction Stop` so a genuine failure surfaces as a non-zero exit.
    // Every interpolation of the name stays inside single-quoted PS literals.
    let script = format!(
        "$procs = @(Get-Process -Name '{safe_name}' -ErrorAction SilentlyContinue); \
         if ($procs.Count -eq 0) {{ throw 'No running process named: {safe_name}' }}; \
         $ids = @($procs.Id); \
         $procs | Stop-Process -Force -ErrorAction Stop; \
         foreach ($id in $ids) {{ Wait-Process -Id $id -Timeout 10 -ErrorAction Stop }}; \
         Write-Output 'Stopped process(es) named: {safe_name}'"
    );
    super::powershell::run_diagnostic(&script).await
}

/// The name to pass to `Get-Process -Name`: a Windows process Name carries no
/// extension, so a trailing ".exe" (any case) is removed. UTF-8 safe (splits on '.').
pub(crate) fn process_query_name(process_name: &str) -> &str {
    process_name
        .rsplit_once('.')
        .filter(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
        .map(|(stem, _)| stem)
        .unwrap_or(process_name)
}

pub(crate) fn validate_kill_target(process_name: &str) -> Result<()> {
    // Stop-Process -Name glob-expands `*?[]` even inside a single-quoted literal
    // (globbing is cmdlet-level, not string interpolation), so `lsass*` would match
    // and kill lsass without ever equalling the exact-match blocklist entry. Refuse
    // wildcards up front — mirrors the has_glob_meta guard in tasks.rs/registry.rs.
    if process_name.contains(['*', '?', '[', ']']) {
        bail!("Process name '{process_name}' contains wildcard characters — refusing");
    }
    let lower = process_name.to_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    if PROTECTED_PROCESSES
        .iter()
        .any(|protected| protected.eq_ignore_ascii_case(lower))
    {
        bail!("Refusing to kill protected process: {process_name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wildcard_process_names_are_refused() {
        // A glob would let Stop-Process match many processes (incl. a protected one
        // whose exact name isn't equal to the pattern) — reject before building a script.
        for bad in ["chrome*", "lsass*", "csrss?", "note[p]ad", "*"] {
            let err = kill(bad).await.unwrap_err();
            assert!(err.to_string().contains("wildcard"), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn protected_processes_are_refused() {
        for name in [
            "lsass",
            "lsass.exe",
            "SVCHOST.EXE",
            "eir-svc.exe",
            "MSMPENG.EXE",
        ] {
            let err = kill(name).await.unwrap_err();
            assert!(err.to_string().contains("protected"), "{err}");
        }
    }

    #[test]
    fn process_query_name_strips_exe_so_get_process_matches() {
        assert_eq!(process_query_name("chrome.exe"), "chrome");
        assert_eq!(process_query_name("Chrome.EXE"), "Chrome");
        assert_eq!(process_query_name("chrome"), "chrome");
        assert_eq!(process_query_name("my.app.exe"), "my.app");
    }

    #[test]
    fn shared_service_hosts_and_eir_are_protected_targets() {
        for name in ["svchost", "eir-svc", "MsMpEng"] {
            assert!(
                PROTECTED_PROCESSES
                    .iter()
                    .any(|protected| protected.eq_ignore_ascii_case(name)),
                "{name} must never be killed"
            );
        }
    }
}
