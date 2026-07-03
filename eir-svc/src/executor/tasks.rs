use anyhow::Result;

pub async fn disable(task_name: &str) -> Result<String> {
    run_task_cmd("Disable-ScheduledTask", task_name).await
}

pub async fn enable(task_name: &str) -> Result<String> {
    run_task_cmd("Enable-ScheduledTask", task_name).await
}

/// Build the PowerShell script for a scheduled-task cmdlet. Kept pure so the
/// injection-safety of the escaping (single-quoted literals only) is unit-testable.
fn build_task_script(cmdlet: &str, task_name: &str) -> String {
    let name_q = super::powershell::ps_single_quote(task_name);
    // Confirmation is a SINGLE-quoted literal: a double-quoted string would evaluate
    // `$(...)`/`$var`, and the escaping only neutralises quote-breakout inside a
    // single-quoted context — echoing the name back double-quoted would be injectable.
    let safe_name = task_name.replace('\'', "''");
    format!(
        "{cmdlet} -TaskName {name_q} -ErrorAction Stop | Out-Null; \
         Write-Output '{cmdlet} succeeded for {safe_name}'"
    )
}

async fn run_task_cmd(cmdlet: &str, task_name: &str) -> Result<String> {
    let script = build_task_script(cmdlet, task_name);
    // Route through the timed, kill_on_drop PowerShell helper. A bare synchronous
    // `Command::output()` here had no timeout, so a stuck Task Scheduler RPC would pin
    // a Tokio blocking-pool thread indefinitely and starve other executor work.
    super::powershell::run_diagnostic(&script).await
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
