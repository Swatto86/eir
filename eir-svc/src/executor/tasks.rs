use anyhow::{bail, Result};

pub async fn disable(task_name: &str) -> Result<String> {
    run_task_cmd("Disable-ScheduledTask", task_name).await
}

pub async fn enable(task_name: &str) -> Result<String> {
    run_task_cmd("Enable-ScheduledTask", task_name).await
}

/// Build the PowerShell script for a scheduled-task cmdlet. Kept pure so the
/// injection-safety of the escaping (single-quoted literals only) is unit-testable.
///
/// A full-path name (`\Folder\Name`) is split into `-TaskPath '\Folder\'` +
/// `-TaskName 'Name'` so exactly that task is targeted — a bare `-TaskName` matches
/// the name in ANY folder, which could toggle a same-named task somewhere else. A bare
/// name (no leading `\`) is therefore resolved first: if `Get-ScheduledTask` returns
/// more than one match, the script `throw`s (surfaced as an error) rather than toggling
/// every same-named task across folders. The `throw` message is a SINGLE-quoted literal
/// so, like every other value here, no user data ever lands in a double-quoted string.
fn build_task_script(cmdlet: &str, task_name: &str) -> String {
    // Confirmation is a SINGLE-quoted literal: a double-quoted string would evaluate
    // `$(...)`/`$var`, and the escaping only neutralises quote-breakout inside a
    // single-quoted context — echoing the name back double-quoted would be injectable.
    let safe_name = task_name.replace('\'', "''");
    if let Some(pos) = task_name.rfind('\\') {
        let (path, name) = task_name.split_at(pos + 1); // path keeps its trailing '\'
        if task_name.starts_with('\\') && !name.is_empty() {
            let path_q = super::powershell::ps_single_quote(path);
            let name_q = super::powershell::ps_single_quote(name);
            return format!(
                "Get-ScheduledTask -TaskPath {path_q} -TaskName {name_q} -ErrorAction Stop \
                 | {cmdlet} -ErrorAction Stop | Out-Null; \
                 Write-Output '{cmdlet} succeeded for {safe_name}'"
            );
        }
    }
    let name_q = super::powershell::ps_single_quote(task_name);
    format!(
        "$t = @(Get-ScheduledTask -TaskName {name_q} -ErrorAction Stop); \
         if ($t.Count -gt 1) {{ throw 'Ambiguous scheduled task — more than one folder \
         has a task with this name; specify the full \\Folder\\Name path' }}; \
         $t | {cmdlet} -ErrorAction Stop | Out-Null; \
         Write-Output '{cmdlet} succeeded for {safe_name}'"
    )
}

async fn run_task_cmd(cmdlet: &str, task_name: &str) -> Result<String> {
    // Get-ScheduledTask/-TaskName wildcard-expand `*?[]` (single-quoting stops
    // injection, not globbing) — refuse metacharacters so one toggle can't fan out
    // across sibling tasks. Mirrors executor::startup's has_glob_meta guard; also
    // covers the legacy bare -TaskName form, which globs the same way.
    if task_name.contains(['*', '?', '[', ']']) {
        bail!("Task name '{task_name}' contains wildcard characters — refusing");
    }
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

    #[tokio::test]
    async fn wildcard_task_names_are_refused() {
        // A glob in either the path or name component must be rejected before any
        // script is built — Get-ScheduledTask would expand it across sibling tasks.
        for bad in [r"\Vendor\Upd*", "Upd?ter", r"\Ven[d]or\Task", "*"] {
            let err = run_task_cmd("Disable-ScheduledTask", bad)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("wildcard"), "{bad}: {err}");
        }
    }

    #[test]
    fn full_path_names_target_path_and_name_separately() {
        let script = build_task_script("Disable-ScheduledTask", r"\Vendor\Sub\Updater");
        assert!(script.contains(r"-TaskPath '\Vendor\Sub\'"));
        assert!(script.contains("-TaskName 'Updater'"));
        assert!(script.contains("Disable-ScheduledTask"));
        assert!(!script.contains('"'));
        // Root-folder task: path '\', name kept whole.
        let root = build_task_script("Enable-ScheduledTask", r"\RootTask");
        assert!(root.contains(r"-TaskPath '\'"));
        assert!(root.contains("-TaskName 'RootTask'"));
        // A bare name (no backslash) resolves first and refuses an ambiguous match
        // (a same-named task in multiple folders) instead of toggling all of them.
        let bare = build_task_script("Disable-ScheduledTask", "JustAName");
        assert!(bare.contains("Get-ScheduledTask -TaskName 'JustAName'"));
        assert!(bare.contains("$t.Count -gt 1"));
        assert!(bare.contains("throw '"));
        assert!(!bare.contains("-TaskPath"));
        assert!(
            !bare.contains('"'),
            "bare-name script has no double quotes: {bare}"
        );
        // A path-form injection attempt stays inside single-quoted literals.
        let inj = build_task_script("Disable-ScheduledTask", r"\Foo\$(calc)");
        assert!(inj.contains(r"'$(calc)'"));
        assert!(!inj.contains('"'));
    }
}
