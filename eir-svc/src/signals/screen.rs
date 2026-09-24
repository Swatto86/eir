//! Source 4 — on-screen errors. The LocalSystem service runs in session 0 and cannot see
//! the user's desktop, so the tray watches for error message boxes and hung ("Not
//! Responding") windows and reports them over the pipe (`UiMsg::ReportScreenError`).
//! This module bounds that untrusted text and de-duplicates repeats before it becomes a
//! signal for the next decision cycle.

use crate::models::ScreenError;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

const MAX_APP_CHARS: usize = 120;
const MAX_TITLE_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 1500;
/// Undelivered reports kept between cycles; the oldest is dropped first.
const BUFFER_CAP: usize = 20;
/// The same message from the same app counts once per window, so a dialog that keeps
/// reappearing (or a tray restart re-reporting it) cannot drive repeated analyses.
const REPEAT_WINDOW_SECS: i64 = 10 * 60;
const SEEN_CAP: usize = 200;

/// Collapse whitespace and control characters to single spaces and cap the length.
fn clean(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in s.split(|c: char| c.is_whitespace() || c.is_control()) {
        if word.is_empty() {
            continue;
        }
        if count > 0 {
            if count >= max_chars {
                break;
            }
            out.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= max_chars {
                break;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

/// Validate and bound one report from the tray. The app is reduced to its file name so
/// a path (and the user name inside it) never reaches the prompt.
pub fn normalise(
    app: &str,
    title: &str,
    text: &str,
    hung: bool,
    at: DateTime<Utc>,
) -> Result<ScreenError, &'static str> {
    let app_name = app.rsplit(['\\', '/']).next().unwrap_or_default();
    let app = clean(app_name, MAX_APP_CHARS);
    if app.is_empty() {
        return Err("screen error has no application");
    }
    let title = clean(title, MAX_TITLE_CHARS);
    let text = if hung {
        String::new()
    } else {
        clean(text, MAX_TEXT_CHARS)
    };
    if !hung && title.is_empty() && text.is_empty() {
        return Err("screen error has no text");
    }
    Ok(ScreenError {
        timestamp: at,
        app,
        title,
        text,
        hung,
    })
}

/// Reports waiting for the next decision cycle, plus the recent-repeat memory.
#[derive(Default)]
pub struct ScreenErrorBuffer {
    pending: VecDeque<ScreenError>,
    seen: VecDeque<(String, i64)>,
}

impl ScreenErrorBuffer {
    /// Queue a report. Returns false (and queues nothing) for a repeat inside the window.
    pub fn push(&mut self, e: ScreenError) -> bool {
        let now = e.timestamp.timestamp();
        let key = format!("{}|{}|{}|{}", e.app, e.title, e.text, e.hung).to_lowercase();
        self.seen.retain(|(_, at)| now - at < REPEAT_WINDOW_SECS);
        if self.seen.iter().any(|(k, _)| *k == key) {
            return false;
        }
        if self.seen.len() >= SEEN_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back((key, now));
        if self.pending.len() >= BUFFER_CAP {
            self.pending.pop_front();
        }
        self.pending.push_back(e);
        true
    }

    /// Hand every queued report to a decision cycle (one-shot, like the other sources).
    pub fn drain(&mut self) -> Vec<ScreenError> {
        self.pending.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn normalise_bounds_text_and_strips_paths() {
        let e = normalise(
            r"C:\Users\Alice\AppData\Local\App\app.exe",
            "  App\r\n Error ",
            &format!("line one\n\n\tline two {}", "x".repeat(5000)),
            false,
            at(1),
        )
        .expect("valid report");
        assert_eq!(e.app, "app.exe");
        assert_eq!(e.title, "App Error");
        assert!(e.text.starts_with("line one line two x"));
        assert_eq!(e.text.chars().count(), MAX_TEXT_CHARS);
        assert!(!e.text.contains("Alice"));
    }

    #[test]
    fn normalise_rejects_empty_reports_but_accepts_a_bare_hang() {
        assert!(normalise("", "t", "x", false, at(1)).is_err());
        assert!(normalise("a.exe", " ", "\n", false, at(1)).is_err());
        let hung = normalise("a.exe", "Editor", "ignored text", true, at(1)).expect("hang");
        assert!(hung.hung);
        assert!(hung.text.is_empty());
    }

    #[test]
    fn buffer_drops_repeats_inside_the_window_and_caps_pending() {
        let mut b = ScreenErrorBuffer::default();
        let e = |t: i64, text: &str| normalise("a.exe", "A", text, false, at(t)).expect("ok");
        assert!(b.push(e(100, "boom")));
        assert!(
            !b.push(e(200, "BOOM")),
            "case-only repeat is the same message"
        );
        assert!(b.push(e(200, "other")));
        assert!(
            b.push(e(100 + REPEAT_WINDOW_SECS, "boom")),
            "window expired"
        );
        assert_eq!(b.drain().len(), 3);
        assert!(b.drain().is_empty(), "drain is one-shot");
        for i in 0..(BUFFER_CAP as i64 + 5) {
            b.push(e(10_000 + i, &format!("m{i}")));
        }
        let kept = b.drain();
        assert_eq!(kept.len(), BUFFER_CAP);
        assert_eq!(kept[0].text, "m5", "oldest reports are dropped first");
    }
}
