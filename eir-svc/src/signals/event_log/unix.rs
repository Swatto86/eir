//! Linux journald collector. Polls on the reused `event_log_poll_interval_secs` cadence
//! (matching the codebase's one collector idiom — event_log/wmi/file_watch are all
//! poll-tick driven) rather than a long-lived `journalctl -f` child.
//!
//! `event_log_channels` is reinterpreted (same field, same type, zero new config key) as
//! an optional list of systemd unit names to scope monitoring to (`-u <name>` per
//! entry); empty — the default — means all of journald at priority ≤ warning. A cursor
//! (`__CURSOR` of the last line read) is persisted to `/var/lib/eir/journal-cursor`
//! after every poll so a restart resumes exactly, not from boot and not losing a gap.

use super::SharedEntries;
use crate::models::EventLogEntry;
use crate::signals::TriggerTx;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};

const BUFFER_CAP: usize = 100;
const MAX_PER_POLL: usize = 100;
const MAX_MESSAGE_CHARS: usize = 2048;
/// systemd's "unit result" message (`Failed with result '...'`), logged by PID 1 at
/// warning priority whenever a unit ends in a non-success state — i.e. a service that
/// crashed or exited with an error. Treated as an Error so a crash triggers a reaction
/// at once, not at the next full system-state poll.
const UNIT_RESULT_MESSAGE_ID: &str = "d9b373ed55a64feb8242e02dbe79a49c";
/// Lines of the failed unit's own log attached for context: the unit's error usually
/// goes to stdout/stderr at info priority, below the collector's warning floor.
const UNIT_TAIL_LINES: &str = "15";
const MAX_UNIT_TAIL_CHARS: usize = 1500;
/// Hard cap on how many lines a single `--after-cursor` poll will ever read.
/// `journalctl` does NOT fail (nonzero exit) when a stored cursor names a boot id that
/// has since rotated out of the retention window — it silently ignores the cursor and
/// returns the WHOLE visible journal instead (reproduced live: a well-formed but
/// unresolvable cursor returned exit 0 and 324k+ lines). A healthy poll on the existing
/// `event_log_poll_interval_secs` cadence (default 30s) should never come close to this
/// many new lines; hitting the cap is therefore treated as a stale/unresolvable cursor,
/// not a genuinely huge burst — see `poll_once`.
const AFTER_CURSOR_LINE_CAP: usize = 500;

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "__CURSOR")]
    cursor: Option<String>,
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime_us: Option<String>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_identifier: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    systemd_unit: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
    #[serde(rename = "MESSAGE_ID")]
    message_id: Option<String>,
    #[serde(rename = "UNIT")]
    unit: Option<String>,
}

/// The unit a systemd "unit result" entry reports as failed, if this is one.
fn failed_unit(raw: &RawEntry) -> Option<&str> {
    (raw.message_id.as_deref() == Some(UNIT_RESULT_MESSAGE_ID))
        .then_some(raw.unit.as_deref())
        .flatten()
        .filter(|u| valid_unit_name(u))
}

/// Unit names come from the journal (untrusted text): plain systemd name characters
/// only, never starting with '-', so the name can never read as a journalctl option.
fn valid_unit_name(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= 256
        && unit
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | ':' | '\\'))
}

/// Most failed units whose own log is attached to one analysis.
const MAX_FAILED_UNITS_CONTEXT: usize = 5;

/// For each currently failed unit that `entries` has nothing about yet, an Error entry
/// carrying the unit's recent log — so every analysis (including a user-requested
/// investigation long after the crash) sees why the unit is down, not only the cycle
/// in which the failure happened. Blocking: call from `spawn_blocking`.
pub fn failed_units_context(
    failed_units: &[String],
    entries: &[EventLogEntry],
) -> Vec<EventLogEntry> {
    failed_units
        .iter()
        .filter(|unit| valid_unit_name(unit))
        .filter(|unit| !entries.iter().any(|e| &e.source == *unit))
        .take(MAX_FAILED_UNITS_CONTEXT)
        .map(|unit| EventLogEntry {
            timestamp: Utc::now(),
            level: "Error".to_string(),
            source: unit.clone(),
            message: match unit_tail(unit) {
                Some(tail) => {
                    format!("{unit} is in the failed state. Recent log of {unit}:\n{tail}")
                }
                None => format!("{unit} is in the failed state (no recent log available)."),
            },
            event_id: 0,
        })
        .collect()
}

/// The failed unit's most recent log lines, any priority (blocking; runs on the
/// collector's blocking poll).
fn unit_tail(unit: &str) -> Option<String> {
    let output = Command::new("journalctl")
        .args(["-u", unit, "-n", UNIT_TAIL_LINES, "-o", "cat", "--no-pager"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let tail: String = text
        .trim()
        .chars()
        .rev()
        .take(MAX_UNIT_TAIL_CHARS)
        .collect();
    let tail: String = tail.chars().rev().collect();
    (!tail.is_empty()).then_some(tail)
}

/// journald's `PRIORITY` (0 emerg .. 7 debug) maps to the two levels the rest of Eir
/// already understands: 0-3 → Error, 4 → Warning, 5-7 are dropped before they ever
/// become an `EventLogEntry` — mirrors the Windows collector's `level_name`.
fn priority_level(priority: &str) -> Option<&'static str> {
    match priority.trim().parse::<u32>() {
        Ok(0..=3) => Some("Error"),
        Ok(4) => Some("Warning"),
        _ => None,
    }
}

/// `MESSAGE` is usually a string, but journald emits an array of byte values for
/// non-UTF-8 payloads (a binary log write) — decode either shape.
fn message_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(bytes) => {
            let raw: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64())
                .map(|b| b as u8)
                .collect();
            String::from_utf8_lossy(&raw).into_owned()
        }
        _ => String::new(),
    }
}

fn char_cap(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Stable across two lines differing only in digits (PID, sequence numbers,
/// timestamps embedded in the message) so one recurring source can't flood a single
/// poll's batch with near-duplicates; differs the moment the message text itself
/// changes. Used only to de-duplicate WITHIN one poll's batch — never across polls
/// (the journald cursor already guarantees each real entry is read exactly once).
fn dedup_fingerprint(source: &str, message: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    fn strip_digits(s: &str) -> String {
        s.chars().filter(|c| !c.is_ascii_digit()).collect()
    }
    let mut hasher = DefaultHasher::new();
    strip_digits(source).hash(&mut hasher);
    strip_digits(message).hash(&mut hasher);
    hasher.finish()
}

fn parse_entry(raw: &RawEntry) -> Option<EventLogEntry> {
    let unit = failed_unit(raw);
    let level = match unit {
        Some(_) => "Error",
        None => priority_level(raw.priority.as_deref()?)?,
    };
    let source = unit
        .map(str::to_string)
        .or_else(|| raw.syslog_identifier.clone())
        .or_else(|| raw.systemd_unit.clone())
        .unwrap_or_else(|| "journal".to_string());
    let message = raw.message.as_ref().map(message_text).unwrap_or_default();
    let message = char_cap(message.trim(), MAX_MESSAGE_CHARS);
    let timestamp = raw
        .realtime_us
        .as_deref()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|us| DateTime::from_timestamp(us / 1_000_000, ((us % 1_000_000) * 1000) as u32))
        .unwrap_or_else(Utc::now);
    Some(EventLogEntry {
        timestamp,
        level: level.to_string(),
        source,
        message,
        event_id: 0,
    })
}

/// Where the journald cursor is persisted: a sibling of the configured audit DB
/// (`[persistence] audit_db`), resolved the same way `main.rs` resolves `audit_db`
/// itself — through `config::resolve`, which honours `EIR_RUNTIME_ROOT` and falls back
/// to beside the executable. A plain hardcoded `/var/lib/eir/journal-cursor` only ever
/// worked in production because that happens to be where the README's manual `mkdir -p
/// /var/lib/eir` step and `audit_db = "/var/lib/eir/eir.db"` put it — any other run
/// (non-root, a different `EIR_RUNTIME_ROOT`, before that manual step) silently lost
/// the cursor every restart instead of erroring loudly.
fn cursor_path() -> PathBuf {
    let audit_db = crate::config::load("config.toml")
        .map(|config| config.persistence.audit_db)
        .unwrap_or_else(|_| "./eir.db".to_string());
    let resolved_db = crate::config::resolve(&audit_db);
    resolved_db
        .parent()
        .map(|dir| dir.join("journal-cursor"))
        .unwrap_or_else(|| crate::config::resolve("journal-cursor"))
}

fn read_cursor() -> Option<String> {
    let cursor = std::fs::read_to_string(cursor_path()).ok()?;
    let cursor = cursor.trim();
    (!cursor.is_empty()).then(|| cursor.to_string())
}

fn write_cursor(cursor: &str) {
    let path = cursor_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, cursor) {
        warn!(
            "Could not persist journald cursor to {}: {error}",
            path.display()
        );
    }
}

fn unit_args(channels: &[String]) -> Vec<String> {
    channels
        .iter()
        .filter(|c| !c.trim().is_empty())
        .flat_map(|c| ["-u".to_string(), c.clone()])
        .collect()
}

fn journalctl_args(channels: &[String], cursor: Option<&str>, priming: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["-o".into(), "json".into(), "--no-pager".into()];
    if let Some(cursor) = cursor {
        args.push("--after-cursor".into());
        args.push(cursor.to_string());
        // Bounds a stale-cursor dump (see AFTER_CURSOR_LINE_CAP's doc comment) — a
        // legitimate incremental poll never gets close to this many lines.
        args.push("-n".into());
        args.push(AFTER_CURSOR_LINE_CAP.to_string());
    } else {
        // Prime: only the single newest entry, to seed the cursor without delivering
        // a historical backlog.
        args.push("-n".into());
        args.push("1".into());
    }
    if !priming {
        args.push("-p".into());
        args.push("warning".into());
    }
    args.extend(unit_args(channels));
    args
}

/// What one `journalctl` invocation's raw output means, decided separately from
/// whether/how to retry — kept pure (no process spawning) so the reseed decision is
/// unit-testable without shelling out. `Reseed` covers both a nonzero exit (e.g.
/// "Failed to seek to cursor", when the log was cleared/rotated/vacuumed past the
/// stored position) and a resolved-but-stale cursor (`AFTER_CURSOR_LINE_CAP` hit).
enum Interpreted {
    Reseed,
    Ready {
        raw_entries: Vec<RawEntry>,
        newest_cursor: Option<String>,
    },
}

fn interpret_journalctl(
    output: &std::process::Output,
    cursor: Option<&str>,
    priming: bool,
) -> Interpreted {
    if !output.status.success() {
        return Interpreted::Reseed;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut raw_entries = Vec::new();
    let mut newest_cursor = cursor.map(str::to_string);
    for line in text.lines() {
        let Ok(raw) = serde_json::from_str::<RawEntry>(line) else {
            continue;
        };
        if let Some(c) = &raw.cursor {
            newest_cursor = Some(c.clone());
        }
        raw_entries.push(raw);
    }
    if !priming && raw_entries.len() >= AFTER_CURSOR_LINE_CAP {
        // journalctl silently ignored an unresolvable `--after-cursor` (a rotated-out
        // boot id) rather than failing — hitting the cap is the only observable signal.
        // Discard this batch entirely (it is not reliably "new since last poll").
        return Interpreted::Reseed;
    }
    Interpreted::Ready {
        raw_entries,
        newest_cursor,
    }
}

/// One poll: reads new lines after `cursor` (or, when `cursor` is `None`, primes by
/// seeking to the current newest entry and returning nothing — exactly like the
/// Windows collector's first-poll prime, so startup never dumps a historical backlog).
/// Returns `(new_entries, new_cursor)`; `None` means the poll itself failed (journalctl
/// missing, a permissions problem) and the caller must leave the stored cursor
/// untouched, matching the Windows "failed open" contract.
///
/// A failed or stale-looking attempt reseeds from `None` exactly once (a loop, not
/// recursion): a persistent failure — a corrupted/inaccessible journal database, not
/// just a missing binary (that returns `None` via `.ok()?` before any of this) — used
/// to recurse with identical arguments on every attempt and had no depth limit, so it
/// could stack-overflow and abort the whole process instead of just this collector.
fn poll_once(channels: &[String], cursor: Option<&str>) -> Option<(Vec<EventLogEntry>, String)> {
    let mut cursor = cursor.map(str::to_string);
    let mut reseeded = false;
    loop {
        let priming = cursor.is_none();
        let args = journalctl_args(channels, cursor.as_deref(), priming);
        let output = Command::new("journalctl").args(&args).output().ok()?;
        let (raw_entries, newest_cursor) =
            match interpret_journalctl(&output, cursor.as_deref(), priming) {
                Interpreted::Reseed => {
                    if reseeded {
                        // Already retried once from a fresh seek and it still failed —
                        // a persistent problem, not a one-off stale cursor. Stop here
                        // instead of looping/recursing forever.
                        warn!(
                            "journald poll failed twice in a row (reseeding did not help); \
                             retaining the last good cursor"
                        );
                        return None;
                    }
                    if !priming {
                        warn!(
                            "journald poll hit the {AFTER_CURSOR_LINE_CAP}-line cap or failed — \
                             treating the stored cursor as stale (rotated out of the retention \
                             window, or unreadable) and reseeding from now rather than \
                             delivering a possibly historical batch"
                        );
                    }
                    reseeded = true;
                    cursor = None;
                    continue;
                }
                Interpreted::Ready {
                    raw_entries,
                    newest_cursor,
                } => (raw_entries, newest_cursor),
            };

        let Some(newest_cursor) = newest_cursor else {
            // Nothing read and no prior cursor either — cannot seed; caller retries.
            return None;
        };
        if priming {
            return Some((Vec::new(), newest_cursor));
        }

        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for raw in &raw_entries {
            let Some(mut entry) = parse_entry(raw) else {
                continue;
            };
            let fingerprint = dedup_fingerprint(&entry.source, &entry.message);
            if !seen.insert(fingerprint) {
                continue;
            }
            if let Some(tail) = failed_unit(raw).and_then(unit_tail) {
                entry.message =
                    format!("{}\nRecent log of {}:\n{tail}", entry.message, entry.source);
            }
            entries.push(entry);
            if entries.len() >= MAX_PER_POLL {
                break;
            }
        }
        return Some((entries, newest_cursor));
    }
}

pub fn spawn(
    channels: Vec<String>,
    poll_interval_secs: u64,
    trigger: TriggerTx,
) -> (SharedEntries, watch::Sender<()>) {
    let shared: SharedEntries = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let shared_clone = shared.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
        let mut cursor = read_cursor();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let channels_clone = channels.clone();
                    let cursor_in = cursor.clone();
                    let polled = tokio::task::spawn_blocking(move || {
                        poll_once(&channels_clone, cursor_in.as_deref())
                    })
                    .await;
                    let (entries, new_cursor) = match polled {
                        Ok(Some(result)) => result,
                        Ok(None) => {
                            warn!("journald poll failed; retaining the last good cursor");
                            continue;
                        }
                        Err(error) => {
                            warn!("journald poll task panicked; retaining the last good cursor: {error}");
                            continue;
                        }
                    };
                    if new_cursor != cursor.clone().unwrap_or_default() {
                        write_cursor(&new_cursor);
                    }
                    cursor = Some(new_cursor);
                    let count = entries.len();
                    let actionable = entries.iter().any(|e| e.level == "Error");
                    if let Ok(mut guard) = shared_clone.lock() {
                        for e in entries {
                            if guard.len() >= BUFFER_CAP {
                                guard.pop_front();
                            }
                            guard.push_back(e);
                        }
                    }
                    if actionable {
                        let _ = trigger.try_send(());
                    }
                    // A quiet poll every 30 seconds is not news for the log.
                    if count > 0 {
                        info!(entries = count, "journald polled");
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    (shared, shutdown_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_maps_error_warning_and_drops_the_rest() {
        assert_eq!(priority_level("0"), Some("Error"));
        assert_eq!(priority_level("3"), Some("Error"));
        assert_eq!(priority_level("4"), Some("Warning"));
        assert_eq!(priority_level("5"), None);
        assert_eq!(priority_level("6"), None);
        assert_eq!(priority_level("7"), None);
        assert_eq!(priority_level("garbage"), None);
    }

    #[test]
    fn message_text_decodes_a_plain_string_and_a_byte_array() {
        assert_eq!(
            message_text(&serde_json::json!("hello")),
            "hello".to_string()
        );
        assert_eq!(
            message_text(&serde_json::json!([104, 105])),
            "hi".to_string()
        );
        assert_eq!(message_text(&serde_json::json!(null)), String::new());
    }

    #[test]
    fn dedup_fingerprint_is_stable_across_digits_and_differs_on_real_changes() {
        let a = dedup_fingerprint("sshd", "Failed password for baduser from 1.2.3.4 port 5555");
        let b = dedup_fingerprint("sshd", "Failed password for baduser from 1.2.3.4 port 9999");
        assert_eq!(
            a, b,
            "PID/port digits alone must not change the fingerprint"
        );
        let c = dedup_fingerprint("sshd", "Accepted publickey for ubuntu from 1.2.3.4");
        assert_ne!(
            a, c,
            "a genuinely different message must change the fingerprint"
        );
    }

    /// Real swatbox `journalctl -o json` samples (fetched live during this session)
    /// decode into the mapped `EventLogEntry` shape.
    #[test]
    fn a_real_swatbox_kernel_sample_maps_to_an_error_level_entry() {
        let sample = r#"{"SYSLOG_IDENTIFIER":"kernel","__REALTIME_TIMESTAMP":"1790328612438306","__CURSOR":"s=9626dfc4ac6241148501266f238bebb1;i=25c932;b=4b13c5c21c9f4e66823e122f43d63961;m=1990d4e388;t=65c4b5cf9c122;x=3e4bc76d146d5774","MESSAGE":"[UFW BLOCK] SRC=78.128.112.6 DST=51.195.201.245","__SEQNUM":"2476338","PRIORITY":"4"}"#;
        let raw: RawEntry = serde_json::from_str(sample).expect("real sample decodes");
        let entry = parse_entry(&raw).expect("mapped entry");
        assert_eq!(entry.level, "Warning");
        assert_eq!(entry.source, "kernel");
        assert!(entry.message.contains("UFW BLOCK"));
    }

    /// Real swatbox `journalctl -o json -u ssh.service` sample.
    #[test]
    fn a_real_swatbox_unit_sample_falls_back_to_systemd_unit_as_source() {
        let sample = r#"{"UNIT":"ssh.service","_SYSTEMD_UNIT":"ssh.service","MESSAGE":"Started ssh.service - OpenBSD Secure Shell server.","_SOURCE_REALTIME_TIMESTAMP":"1790218813199405","__REALTIME_TIMESTAMP":"1790218813199405","PRIORITY":"6"}"#;
        let raw: RawEntry = serde_json::from_str(sample).expect("real sample decodes");
        // PRIORITY 6 (info) is dropped — matches the Windows collector's Error/Warning-only floor.
        assert!(parse_entry(&raw).is_none());
    }

    #[test]
    fn unit_args_build_repeated_dash_u_flags_and_skip_blanks() {
        let channels = vec![
            "caddy.service".to_string(),
            String::new(),
            "ssh.service".to_string(),
        ];
        assert_eq!(
            unit_args(&channels),
            vec!["-u", "caddy.service", "-u", "ssh.service"]
        );
        assert!(unit_args(&[]).is_empty());
    }

    /// A real exit status, produced by actually running a trivial command — simplest
    /// portable way to get a genuine `std::process::ExitStatus` for a test `Output`
    /// without depending on platform-specific raw-code construction.
    fn fake_status(success: bool) -> std::process::ExitStatus {
        let cmd = if success { "true" } else { "false" };
        std::process::Command::new(cmd)
            .status()
            .unwrap_or_else(|e| panic!("run '{cmd}' to build a test exit status: {e}"))
    }

    /// Regression test for the bug the AFTER_CURSOR_LINE_CAP reseed exists to catch:
    /// journalctl silently ignoring an unresolvable `--after-cursor` and dumping far
    /// more than a healthy incremental poll ever would, instead of failing outright.
    #[test]
    fn interpret_journalctl_reseeds_when_the_line_cap_is_hit() {
        let line = r#"{"__CURSOR":"c","PRIORITY":"4","MESSAGE":"x"}"#;
        let stdout = vec![line; AFTER_CURSOR_LINE_CAP].join("\n");
        let output = std::process::Output {
            status: fake_status(true),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            interpret_journalctl(&output, Some("stale-cursor"), false),
            Interpreted::Reseed
        ));
    }

    #[test]
    fn interpret_journalctl_reseeds_on_nonzero_exit_status() {
        let output = std::process::Output {
            status: fake_status(false),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            interpret_journalctl(&output, Some("some-cursor"), false),
            Interpreted::Reseed
        ));
    }

    #[test]
    fn interpret_journalctl_is_ready_under_the_line_cap() {
        let line = r#"{"__CURSOR":"c","PRIORITY":"4","MESSAGE":"x"}"#;
        let stdout = vec![line; AFTER_CURSOR_LINE_CAP - 1].join("\n");
        let output = std::process::Output {
            status: fake_status(true),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        };
        match interpret_journalctl(&output, Some("cursor"), false) {
            Interpreted::Ready {
                raw_entries,
                newest_cursor,
            } => {
                assert_eq!(raw_entries.len(), AFTER_CURSOR_LINE_CAP - 1);
                assert_eq!(newest_cursor.as_deref(), Some("c"));
            }
            Interpreted::Reseed => panic!("expected Ready, got Reseed"),
        }
    }

    #[test]
    fn interpret_journalctl_never_reseeds_while_priming_even_with_no_output() {
        let output = std::process::Output {
            status: fake_status(true),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            interpret_journalctl(&output, None, true),
            Interpreted::Ready { .. }
        ));
    }

    #[test]
    fn a_crashed_unit_is_an_error_attributed_to_the_unit() {
        // Real swatbox shape: systemd (PID 1) reporting a service that exited with an error.
        let raw: RawEntry = serde_json::from_str(
            r#"{"MESSAGE_ID":"d9b373ed55a64feb8242e02dbe79a49c","UNIT":"report-exporter.service","SYSLOG_IDENTIFIER":"systemd","PRIORITY":"4","MESSAGE":"report-exporter.service: Failed with result 'exit-code'.","__REALTIME_TIMESTAMP":"1790341213000000"}"#,
        )
        .expect("decode");
        let e = parse_entry(&raw).expect("kept");
        assert_eq!(e.level, "Error");
        assert_eq!(e.source, "report-exporter.service");
        // Any other warning stays a Warning attributed to its identifier.
        let other: RawEntry =
            serde_json::from_str(r#"{"SYSLOG_IDENTIFIER":"kernel","PRIORITY":"4","MESSAGE":"x"}"#)
                .expect("decode");
        let o = parse_entry(&other).expect("kept");
        assert_eq!(o.level, "Warning");
        assert_eq!(o.source, "kernel");
        // A unit name that could read as a journalctl option is not trusted.
        assert!(!valid_unit_name("-n9999"));
        assert!(!valid_unit_name("a b"));
        assert!(valid_unit_name("getty@tty1.service"));
    }
}
