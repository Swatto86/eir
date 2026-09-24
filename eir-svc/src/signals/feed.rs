//! The dashboard's "What Eir noticed" feed: a bounded, display-only record of the
//! errors each collector delivered, so the user sees Eir pick a problem up the moment it
//! happens and can ask it to explain or fix that exact item.

use crate::models::{ScreenError, SignalSnapshot};
use eir_proto::SignalView;
use std::collections::VecDeque;

const FEED_CAP: usize = 30;
const MAX_SUMMARY_CHARS: usize = 240;
/// An identical item already in the feed this recently is not repeated.
const REPEAT_WINDOW_SECS: i64 = 10 * 60;

fn one_line(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= MAX_SUMMARY_CHARS {
        return joined;
    }
    let mut out: String = joined.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    out.push('…');
    out
}

/// Error-level items from one cycle's drained signals (warnings are too noisy to list).
/// Screen errors are added when reported, not here, so they are not listed twice.
pub fn from_snapshot(snap: &SignalSnapshot) -> Vec<SignalView> {
    let mut items = Vec::new();
    for e in snap.event_log.iter().filter(|e| e.level == "Error") {
        items.push(SignalView {
            at: e.timestamp.timestamp(),
            source: "event_log".into(),
            app: one_line(&e.source),
            summary: one_line(&format!("Event {}: {}", e.event_id, e.message)),
        });
    }
    for fc in &snap.file_changes {
        let Some(le) = &fc.log_event else { continue };
        if !matches!(le.severity.as_str(), "ERROR" | "FATAL") {
            continue;
        }
        let first = le
            .error_snippets
            .iter()
            .flat_map(|s| s.lines())
            .find(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("error") || l.contains("fatal") || l.contains("exception")
            })
            .or_else(|| le.error_snippets.first().map(String::as_str))
            .unwrap_or("error written to its log");
        items.push(SignalView {
            at: fc.timestamp.timestamp(),
            source: "app_log".into(),
            app: one_line(&le.program),
            summary: one_line(first),
        });
    }
    items
}

pub fn screen_view(e: &ScreenError) -> SignalView {
    let summary = if e.hung {
        format!("Stopped responding: {}", e.title)
    } else if e.title.is_empty() {
        e.text.clone()
    } else if e.text.is_empty() {
        e.title.clone()
    } else {
        format!("{} — {}", e.title, e.text)
    };
    SignalView {
        at: e.timestamp.timestamp(),
        source: if e.hung { "hung" } else { "screen" }.into(),
        app: e.app.clone(),
        summary: one_line(&summary),
    }
}

/// Add items newest-first, skipping repeats of an item already listed recently.
pub fn push(feed: &mut VecDeque<SignalView>, items: Vec<SignalView>) {
    for item in items {
        let repeat = feed.iter().any(|f| {
            f.source == item.source
                && f.app == item.app
                && f.summary == item.summary
                && (item.at - f.at).abs() < REPEAT_WINDOW_SECS
        });
        if repeat {
            continue;
        }
        feed.push_front(item);
        feed.truncate(FEED_CAP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EventLogEntry, FileChange, LogEvent, SystemState};
    use chrono::{TimeZone, Utc};

    fn snap() -> SignalSnapshot {
        let t = Utc.timestamp_opt(1_000, 0).single().expect("ts");
        let event = |level: &str| EventLogEntry {
            timestamp: t,
            level: level.into(),
            source: "Application Error".into(),
            message: "Faulting application name: app.exe,\n exception 0xc0000005".into(),
            event_id: 1000,
        };
        let change = |severity: &str| FileChange {
            path: "C:\\x.log".into(),
            kind: "modify".into(),
            size_bytes: 1,
            timestamp: t,
            log_event: Some(LogEvent {
                program: "Backup".into(),
                log_path: "C:\\x.log".into(),
                severity: severity.into(),
                error_snippets: vec!["starting\nERROR disk full\nstopping".into()],
                content_excerpt: String::new(),
            }),
        };
        SignalSnapshot {
            timestamp: t,
            event_log: vec![event("Error"), event("Warning")],
            file_changes: vec![change("ERROR"), change("WARN")],
            system_state: SystemState::default(),
            decision_history: vec![],
            screen_errors: vec![],
            user_report: None,
        }
    }

    #[test]
    fn lists_only_error_level_items_with_a_readable_summary() {
        let items = from_snapshot(&snap());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].source, "event_log");
        assert_eq!(
            items[0].summary,
            "Event 1000: Faulting application name: app.exe, exception 0xc0000005"
        );
        assert_eq!(items[1].source, "app_log");
        assert_eq!(items[1].app, "Backup");
        assert_eq!(items[1].summary, "ERROR disk full");
    }

    #[test]
    fn push_is_newest_first_bounded_and_skips_recent_repeats() {
        let mut feed = VecDeque::new();
        let item = |at: i64, summary: &str| SignalView {
            at,
            source: "screen".into(),
            app: "a.exe".into(),
            summary: summary.into(),
        };
        push(&mut feed, vec![item(1, "x"), item(2, "y"), item(3, "x")]);
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].summary, "y");
        push(&mut feed, vec![item(1 + REPEAT_WINDOW_SECS, "x")]);
        assert_eq!(feed[0].summary, "x", "an old repeat is listed again");
        push(
            &mut feed,
            (0..50)
                .map(|i| item(100_000 + i, &format!("m{i}")))
                .collect(),
        );
        assert_eq!(feed.len(), FEED_CAP);
        assert_eq!(feed[0].summary, "m49");
    }

    #[test]
    fn screen_view_describes_dialogs_and_hangs() {
        let t = Utc.timestamp_opt(5, 0).single().expect("ts");
        let dialog = ScreenError {
            timestamp: t,
            app: "outlook.exe".into(),
            title: "Microsoft Outlook".into(),
            text: "Cannot open the folder.".into(),
            hung: false,
        };
        assert_eq!(
            screen_view(&dialog).summary,
            "Microsoft Outlook — Cannot open the folder."
        );
        let hung = ScreenError {
            hung: true,
            text: String::new(),
            title: "Report.docx - Word".into(),
            ..dialog
        };
        let v = screen_view(&hung);
        assert_eq!(v.source, "hung");
        assert_eq!(v.summary, "Stopped responding: Report.docx - Word");
        assert_eq!(
            one_line(&"x".repeat(400)).chars().count(),
            MAX_SUMMARY_CHARS
        );
    }
}
