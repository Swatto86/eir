use crate::models::LogEvent;
use std::path::Path;

const ERROR_KEYWORDS: &[&str] = &[
    "error", "fatal", "critical", "exception", "crash",
    "panic", "unhandled", "traceback", "stack trace", "access violation",
    "segfault", "corrupt", "aborted",
];

const WARN_KEYWORDS: &[&str] = &[
    "warn", "warning", "deprecated", "failed", "failure", "timeout",
    "refused", "denied", "unavailable",
];

/// Parse a log file's content and extract structured diagnostic information.
pub fn parse(path: &Path, content: &str) -> LogEvent {
    let program = extract_program(path);
    let (error_snippets, severity) = extract_errors(content);
    LogEvent {
        program,
        log_path: path.to_string_lossy().into_owned(),
        severity,
        error_snippets,
    }
}

// ── Program name ──────────────────────────────────────────────────────────────

fn extract_program(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .filter_map(|c| {
            if let std::path::Component::Normal(s) = c {
                s.to_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    for (i, comp) in components.iter().enumerate() {
        let lower = comp.to_lowercase();

        // C:\Program Files[\ (x86)]\<App>
        if lower == "program files" || lower == "program files (x86)" {
            if let Some(app) = components.get(i + 1) {
                return app.clone();
            }
        }

        // C:\ProgramData\<App>
        if lower == "programdata" {
            if let Some(app) = components.get(i + 1) {
                return app.clone();
            }
        }

        // C:\Windows\Logs\<Subsystem>
        if lower == "logs" && components.get(i.wrapping_sub(1)).map(|c| c.to_lowercase()).as_deref() == Some("windows") {
            if let Some(sub) = components.get(i + 1) {
                return format!("Windows {sub}");
            }
        }

        // C:\Users\*\AppData\(Local|Roaming|LocalLow)\<App>
        if matches!(lower.as_str(), "local" | "roaming" | "locallow")
            && components.get(i.wrapping_sub(1)).map(|c| c.to_lowercase()).as_deref() == Some("appdata")
        {
            if let Some(app) = components.get(i + 1) {
                return app.clone();
            }
        }
    }

    // Fallback: parent directory name
    path.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string())
}

// ── Error extraction ──────────────────────────────────────────────────────────

fn extract_errors(content: &str) -> (Vec<String>, String) {
    let lines: Vec<&str> = content.lines().collect();
    let mut snippets: Vec<String> = Vec::new();
    let mut severity = "INFO".to_string();
    let mut next_allowed = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();

        let is_error = ERROR_KEYWORDS.iter().any(|k| lower.contains(k));
        let is_warn = !is_error && WARN_KEYWORDS.iter().any(|k| lower.contains(k));

        if !is_error && !is_warn {
            continue;
        }

        // Update severity ceiling
        if severity != "FATAL" {
            if lower.contains("fatal") || lower.contains("critical") || lower.contains("crash")
                || lower.contains("access violation") || lower.contains("aborted")
            {
                severity = "FATAL".to_string();
            } else if severity != "ERROR" && is_error {
                severity = "ERROR".to_string();
            } else if severity == "INFO" && is_warn {
                severity = "WARN".to_string();
            }
        }

        // Collect up to 5 non-overlapping snippets (1 line before + error + 2 after)
        if snippets.len() < 5 && i >= next_allowed {
            let start = i.saturating_sub(1);
            let end = (i + 3).min(lines.len());
            next_allowed = end;
            snippets.push(lines[start..end].join("\n"));
        }
    }

    (snippets, severity)
}
