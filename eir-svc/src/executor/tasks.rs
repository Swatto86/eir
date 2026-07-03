use anyhow::Result;

pub fn disable(task_name: &str) -> Result<String> {
    run_task_cmd("Disable-ScheduledTask", task_name)
}

pub fn enable(task_name: &str) -> Result<String> {
    run_task_cmd("Enable-ScheduledTask", task_name)
}

/// Build the PowerShell script for a scheduled-task cmdlet. Kept pure so the
/// injection-safety of the escaping (single-quoted literals only) is unit-testable.
fn build_task_script(cmdlet: &str, task_name: &str) -> String {
    let safe_name = task_name.replace('\'', "''");
    // Confirmation is a SINGLE-quoted literal: a double-quoted string would evaluate
    // `$(...)`/`$var`, and the escaping above only neutralises quote-breakout inside a
    // single-quoted context — echoing the name back double-quoted would be injectable.
    format!(
        "{cmdlet} -TaskName '{safe_name}' -ErrorAction Stop | Out-Null; \
         Write-Output '{cmdlet} succeeded for {safe_name}'"
    )
}

fn run_task_cmd(cmdlet: &str, task_name: &str) -> Result<String> {
    let script = build_task_script(cmdlet, task_name);

    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NonInteractive",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if out.status.success() {
        Ok(stdout.trim().to_string())
    } else {
        anyhow::bail!("{cmdlet} failed for '{task_name}': {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_script_never_double_quotes_the_name() {
        // A `$(...)` payload must be inert: the generated script must not wrap it in a
        // double-quoted string (where PowerShell would evaluate the subexpression).
        let script = build_task_script("Disable-ScheduledTask", "$(calc.exe)");
        assert!(!script.contains('"'), "no double-quoted strings: {script}");
        // The value only ever appears inside single quotes (literal in PowerShell).
        assert!(script.contains("'$(calc.exe)'"));
    }

    #[test]
    fn task_script_escapes_embedded_single_quotes() {
        let script = build_task_script("Enable-ScheduledTask", "O'Brien");
        // A single quote is doubled so it can't break out of the literal.
        assert!(script.contains("O''Brien"));
        assert!(!script.contains('"'));
    }
}
