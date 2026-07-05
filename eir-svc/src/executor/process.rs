use anyhow::{bail, Result};

const PROTECTED_PROCESSES: &[&str] = &[
    "System", "Idle", "lsass", "winlogon", "csrss", "smss", "wininit", "services", "ntoskrnl",
];

pub async fn kill(process_name: &str) -> Result<String> {
    // Stop-Process -Name glob-expands `*?[]` even inside a single-quoted literal
    // (globbing is cmdlet-level, not string interpolation), so `lsass*` would match
    // and kill lsass without ever equalling the exact-match blocklist entry. Refuse
    // wildcards up front — mirrors the has_glob_meta guard in tasks.rs/registry.rs.
    if process_name.contains(['*', '?', '[', ']']) {
        bail!("Process name '{process_name}' contains wildcard characters — refusing");
    }
    let lower = process_name.to_lowercase();
    if PROTECTED_PROCESSES
        .iter()
        .any(|&p| p.to_lowercase() == lower)
    {
        bail!("Refusing to kill protected process: {process_name}");
    }
    let safe_name = process_name.replace('\'', "''");
    let script = format!(
        "Stop-Process -Name '{safe_name}' -Force -ErrorAction SilentlyContinue; \
         Write-Output 'Kill signal sent to: {safe_name}'"
    );
    super::powershell::run_diagnostic(&script).await
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
        let err = kill("lsass").await.unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
    }
}
