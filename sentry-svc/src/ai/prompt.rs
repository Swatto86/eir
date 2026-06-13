use crate::models::{LogEvent, PastDecision, SignalSnapshot};

pub fn build(
    snapshot: &SignalSnapshot,
    history: &[PastDecision],
    feedback_summary: Option<&str>,
) -> String {
    let mut snapshot_value = serde_json::to_value(snapshot).unwrap_or_default();
    if let Some(obj) = snapshot_value.as_object_mut() {
        obj.remove("decision_history");
    }
    let snapshot_json = serde_json::to_string_pretty(&snapshot_value).unwrap_or_default();
    let history_json = serde_json::to_string_pretty(history).unwrap_or_default();

    let log_events_section = format_log_events(snapshot);

    let feedback_section = match feedback_summary {
        Some(s) if !s.is_empty() && s != "No execution history yet." => format!(
            "\nRECENT EXECUTION FEEDBACK (calibrate confidence from past outcomes):\n{s}\n\
             If a similar action failed before, lower confidence. \
             If it improved the system, raise it.\n"
        ),
        _ => String::new(),
    };

    format!(
        r#"You are Sentry, an autonomous Windows system repair agent running on a home PC.
Your job: analyze system signals, diagnose problems, and propose targeted fixes.
{log_events_section}
CURRENT SYSTEM STATE (full snapshot):
{snapshot_json}

RECENT DECISION HISTORY (last 5):
{history_json}
{feedback_section}
AVAILABLE FIX ACTIONS (use the exact action key and fields shown):
  service_restart / service_stop / service_start: {{"action": "...", "service_name": "..."}}
  log_cleanup:           {{"action": "log_cleanup", "path": "C:\\Logs\\App", "days_old": 7}}
  disk_cleanup:          {{"action": "disk_cleanup", "target": "temp"}}           -- target: temp|prefetch
  powershell_diagnostic: {{"action": "powershell_diagnostic", "script": "..."}}
  task_disable / task_enable: {{"action": "...", "task_name": "..."}}
  registry_reset:        {{"action": "registry_reset", "key_path": "HKLM:\\...", "value_name": "...", "value_data": "..."}}
  network_diagnostic:    {{"action": "network_diagnostic", "command": "flush_dns"}}  -- flush_dns|release_renew|reset_tcp|reset_winsock
  driver_disable / driver_enable: {{"action": "...", "driver_name": "..."}}
  software_uninstall:    {{"action": "software_uninstall", "package_name": "..."}}
  bcd_edit:              {{"action": "bcd_edit", "element": "timeout", "value": "30"}}  -- safe elements: timeout|bootmenupolicy|bootstatuspolicy|recoveryenabled|quietboot|nx
  process_kill:          {{"action": "process_kill", "process_name": "Notepad"}}
  file_delete:           {{"action": "file_delete", "path": "C:\\Users\\...\\AppData\\Local\\App\\cache\\file.db"}}
                         -- Deletes a SINGLE FILE only (not directories). Use for corrupted caches,
                            lock files, bad config files, or crash dumps identified in log events.

Analyze thoroughly. For each LOG EVENT above, draw on your knowledge of that program's
known issues and common fixes — including specific registry keys, cache paths, config
locations, and documented workarounds. Propose the exact fix path for that program.

Report at most the 5 most important problems, ordered by severity. Omit trivia and
anything you would rate below 0.6 confidence. If the system is healthy, return an empty
problems list. Keep every text field to one or two sentences.

For EACH problem:
1. Diagnosis: specific and actionable
2. Root cause: why it is happening
3. Confidence: 0.0-1.0 (lower if similar action failed before; higher if past fix worked)
4. Proposed fix: one action from the list above with exact values
5. Reasoning: why this fix resolves this specific error in this specific program
6. Side effects: what might break
7. Undo instructions: how to revert

Respond ONLY with valid JSON (no markdown, no preamble):
{{
  "analysis": "Overall system health summary",
  "problems": [
    {{
      "diagnosis": "...",
      "root_cause": "...",
      "confidence": 0.90,
      "proposed_fix": {{
        "action": "file_delete",
        "path": "C:\\Users\\<USERNAME>\\AppData\\Local\\Microsoft\\Teams\\Cache"
      }},
      "reasoning": "...",
      "side_effects": "...",
      "undo_instructions": "..."
    }}
  ]
}}"#,
        log_events_section = log_events_section,
        snapshot_json = snapshot_json,
        history_json = history_json,
        feedback_section = feedback_section,
    )
}

fn format_log_events(snapshot: &SignalSnapshot) -> String {
    let events: Vec<&LogEvent> = snapshot
        .file_changes
        .iter()
        .filter_map(|fc| fc.log_event.as_ref())
        .filter(|le| !le.error_snippets.is_empty())
        .collect();

    if events.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\nLOG EVENTS — files written since last cycle (highest priority for diagnosis):\n",
    );
    let bar = "─".repeat(72);

    for ev in &events {
        out.push_str(&format!("{bar}\n"));
        out.push_str(&format!("  Program  : {}\n", ev.program));
        out.push_str(&format!("  File     : {}\n", ev.log_path));
        out.push_str(&format!("  Severity : {}\n", ev.severity));
        out.push_str("  Errors   :\n");
        for snippet in &ev.error_snippets {
            for line in snippet.lines() {
                out.push_str(&format!("    {line}\n"));
            }
            out.push('\n');
        }
    }
    out.push_str(&format!("{bar}\n"));
    out
}
