use crate::models::LogEvent;
use std::path::Path;

const ERROR_KEYWORDS: &[&str] = &[
    "error",
    "fatal",
    "critical",
    "exception",
    "crash",
    "panic",
    "unhandled",
    "traceback",
    "stack trace",
    "access violation",
    "segfault",
    "corrupt",
    "aborted",
];

const WARN_KEYWORDS: &[&str] = &[
    "warn",
    "warning",
    "deprecated",
    "failed",
    "failure",
    "timeout",
    "refused",
    "denied",
    "unavailable",
];

/// Largest raw excerpt of a file we hand to the AI. Big enough to show a small
/// config/cache file in full, small enough not to bloat the prompt for a large
/// rolling log (where the keyword snippets carry the signal anyway).
const MAX_EXCERPT_CHARS: usize = 2500;

/// Error excerpts kept per log event.
pub const MAX_SNIPPETS: usize = 5;

/// Longest line kept in an excerpt. A minified or single-line file otherwise put tens
/// of KiB into every snapshot, prompt and audit row.
const MAX_LINE_CHARS: usize = 300;

/// Parse newly read log text and extract structured diagnostic information.
/// `is_new` is asked once per matching line and answers whether that line has not
/// been reported recently; repeats count toward neither the severity nor the
/// excerpts, so an app that keeps writing the same line is reported once.
pub fn parse(path: &Path, content: &str, mut is_new: impl FnMut(&str) -> bool) -> LogEvent {
    let program = extract_program(path);
    let (error_snippets, severity) = extract_errors(content, &mut is_new);
    LogEvent {
        program,
        log_path: path.to_string_lossy().into_owned(),
        severity,
        error_snippets,
        content_excerpt: excerpt(content),
    }
}

/// The first `MAX_EXCERPT_CHARS` characters of the file, with a marker when
/// truncated so the model knows it is seeing only the head.
fn excerpt(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= MAX_EXCERPT_CHARS {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(MAX_EXCERPT_CHARS).collect();
        format!("{head}\n…[truncated]")
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
        if lower == "logs"
            && components
                .get(i.wrapping_sub(1))
                .map(|c| c.to_lowercase())
                .as_deref()
                == Some("windows")
        {
            if let Some(sub) = components.get(i + 1) {
                return format!("Windows {sub}");
            }
        }

        // C:\Users\*\AppData\(Local|Roaming|LocalLow)\<App>
        if matches!(lower.as_str(), "local" | "roaming" | "locallow")
            && components
                .get(i.wrapping_sub(1))
                .map(|c| c.to_lowercase())
                .as_deref()
                == Some("appdata")
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

/// Rank of a severity token, for keeping the highest of several.
pub fn severity_rank(severity: &str) -> u8 {
    match severity {
        "FATAL" => 3,
        "ERROR" => 2,
        "WARN" => 1,
        _ => 0,
    }
}

fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut clipped: String = line.chars().take(MAX_LINE_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

fn extract_errors(content: &str, is_new: &mut impl FnMut(&str) -> bool) -> (Vec<String>, String) {
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
        // A line already reported for this file is old news, not a new finding.
        if !is_new(line) {
            continue;
        }

        // Update severity ceiling
        if severity != "FATAL" {
            if lower.contains("fatal")
                || lower.contains("critical")
                || lower.contains("crash")
                || lower.contains("access violation")
                || lower.contains("aborted")
            {
                severity = "FATAL".to_string();
            } else if severity != "ERROR" && is_error {
                severity = "ERROR".to_string();
            } else if severity == "INFO" && is_warn {
                severity = "WARN".to_string();
            }
        }

        // Collect up to MAX_SNIPPETS non-overlapping snippets (1 line before + error + 2 after)
        if snippets.len() < MAX_SNIPPETS && i >= next_allowed {
            let start = i.saturating_sub(1);
            let end = (i + 3).min(lines.len());
            // The next snippet starts one line BEFORE its match, so the next match
            // must be at `end + 1` for its window to clear this one; `end` alone let
            // line `end - 1` appear in both snippets.
            next_allowed = end + 1;
            snippets.push(
                lines[start..end]
                    .iter()
                    .map(|line| clip_line(line))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    (snippets, severity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_new(_: &str) -> bool {
        true
    }

    #[test]
    fn lines_already_reported_are_neither_excerpted_nor_counted() {
        let seen = ["FATAL crash in renderer"];
        let mut unseen = |line: &str| !seen.contains(&line);
        let (snippets, severity) = extract_errors(
            "ok\nFATAL crash in renderer\nfine\nWARN retry failed\nend\n",
            &mut unseen,
        );
        assert_eq!(
            severity, "WARN",
            "the old FATAL line no longer raises the severity"
        );
        assert_eq!(snippets.len(), 1);
        assert!(snippets[0].contains("WARN retry failed"));

        let (snippets, severity) = extract_errors("FATAL crash in renderer\n", &mut unseen);
        assert!(snippets.is_empty());
        assert_eq!(severity, "INFO", "only repeats means nothing new to report");
    }

    #[test]
    fn excerpt_lines_are_clipped() {
        let long = format!("ERROR {}", "x".repeat(10_000));
        let (snippets, _) = extract_errors(&long, &mut always_new);
        assert_eq!(snippets[0].chars().count(), MAX_LINE_CHARS);
        assert!(snippets[0].ends_with('…'));
    }

    #[test]
    fn severity_ranks_order_fatal_over_error_over_warn() {
        assert!(severity_rank("FATAL") > severity_rank("ERROR"));
        assert!(severity_rank("ERROR") > severity_rank("WARN"));
        assert!(severity_rank("WARN") > severity_rank("INFO"));
    }

    #[test]
    fn error_snippets_do_not_share_a_line() {
        // Matches three lines apart: each window reaches one line back, so the
        // previous window's last line used to be duplicated into the next snippet.
        let (snippets, severity) =
            extract_errors("a\nb\nERROR one\nc\nd\nERROR two\ne\nf\n", &mut always_new);
        assert_eq!(severity, "ERROR");
        assert!(
            snippets.iter().any(|s| s.contains("ERROR one")),
            "the first match must still be captured"
        );
        let mut seen: Vec<&str> = Vec::new();
        for line in snippets.iter().flat_map(|s| s.lines()) {
            assert!(!seen.contains(&line), "line {line:?} is in two snippets");
            seen.push(line);
        }
    }
}
