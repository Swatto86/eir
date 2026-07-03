use crate::models::EventLogEntry;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::System::EventLog::{
    CloseEventLog, OpenEventLogW, ReadEventLogW, EVENTLOGRECORD, READ_EVENT_LOG_READ_FLAGS,
    REPORT_EVENT_TYPE,
};

const RING_SIZE: usize = 20;
/// Cap on the accumulate-until-drained buffer (multiple polls can land between
/// decision cycles); oldest entries are dropped first.
const BUFFER_CAP: usize = 100;

// READ_EVENT_LOG_READ_FLAGS values
const SEQUENTIAL_BACKWARDS: READ_EVENT_LOG_READ_FLAGS = READ_EVENT_LOG_READ_FLAGS(0x0008 | 0x0001);

// REPORT_EVENT_TYPE values
const ETYPE_ERROR: REPORT_EVENT_TYPE = REPORT_EVENT_TYPE(0x0001);
const ETYPE_WARNING: REPORT_EVENT_TYPE = REPORT_EVENT_TYPE(0x0002);
const ETYPE_INFORMATION: REPORT_EVENT_TYPE = REPORT_EVENT_TYPE(0x0004);

pub type SharedEntries = Arc<Mutex<VecDeque<EventLogEntry>>>;

fn win32_time_to_datetime(seconds_since_1970: u32) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds_since_1970 as i64, 0).unwrap_or_else(Utc::now)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn level_name(event_type: REPORT_EVENT_TYPE) -> Option<&'static str> {
    match event_type {
        ETYPE_ERROR => Some("Error"),
        ETYPE_WARNING => Some("Warning"),
        ETYPE_INFORMATION => Some("Information"),
        _ => None,
    }
}

/// Read entries newer than `last_record` from a single event log channel.
/// Returns the new entries (newest first) and the highest record number seen.
///
/// When `prime` is true (the channel's first poll), the cursor is seeded to the
/// current newest record WITHOUT returning any entries — otherwise startup would
/// dump up to `RING_SIZE` possibly days-old, already-resolved errors and fire the
/// reactive trigger on stale data. A `RecordNumber` reset (log cleared / retention
/// rollover / u32 wrap — the newest record is now *below* the stored cursor) is
/// detected and treated like a re-prime, so post-clear events aren't silently
/// dropped forever by the "already delivered" guard.
fn read_channel_since(channel: &str, last_record: u32, prime: bool) -> (Vec<EventLogEntry>, u32) {
    let channel_w = wide(channel);
    let handle = match unsafe { OpenEventLogW(PCWSTR::null(), PCWSTR(channel_w.as_ptr())) } {
        Ok(h) => h,
        Err(e) => {
            warn!("Failed to open event log channel {channel}: {e}");
            return (vec![], last_record);
        }
    };

    let mut entries = Vec::new();
    let mut buf = vec![0u8; 65536];
    let mut new_max_record = last_record;
    let mut first_seen = false;
    // Set once we know we should deliver nothing this pass and only reseed the cursor.
    let mut reseed_only = prime;
    let mut done = false;

    while !done {
        let mut bytes_read: u32 = 0;
        let mut min_bytes_needed: u32 = 0;

        if unsafe {
            ReadEventLogW(
                handle,
                SEQUENTIAL_BACKWARDS,
                0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut bytes_read,
                &mut min_bytes_needed,
            )
        }
        .is_err()
        {
            break;
        }

        let mut offset = 0usize;
        while offset < bytes_read as usize {
            let record = unsafe { &*(buf.as_ptr().add(offset) as *const EVENTLOGRECORD) };

            if record.Length == 0 {
                done = true;
                break;
            }

            // The first record is the newest (we read backwards): it defines the
            // channel's current high-water mark and reveals a log reset.
            if !first_seen {
                first_seen = true;
                new_max_record = record.RecordNumber;
                if !prime && last_record > 0 && record.RecordNumber < last_record {
                    // Cursor is ahead of the newest record — the log was cleared or
                    // wrapped. Reseed to the new max and deliver nothing this pass.
                    reseed_only = true;
                }
            }

            // Priming or reseeding: we only needed the newest record number.
            if reseed_only {
                done = true;
                break;
            }

            // We read newest-first; stop once we reach records already delivered.
            if last_record > 0 && record.RecordNumber <= last_record {
                done = true;
                break;
            }

            if record.RecordNumber > new_max_record {
                new_max_record = record.RecordNumber;
            }

            if let Some(level) = level_name(record.EventType) {
                let source_ptr = unsafe {
                    (record as *const EVENTLOGRECORD as *const u8)
                        .add(std::mem::size_of::<EVENTLOGRECORD>())
                        as *const u16
                };
                let source = unsafe {
                    let mut len = 0usize;
                    while *source_ptr.add(len) != 0 {
                        len += 1;
                    }
                    String::from_utf16_lossy(std::slice::from_raw_parts(source_ptr, len))
                };

                let timestamp = win32_time_to_datetime(record.TimeGenerated);
                entries.push(EventLogEntry {
                    timestamp,
                    level: level.to_string(),
                    source,
                    // Full message extraction requires loading provider DLLs; event ID is sufficient for Phase 1
                    message: format!("EventID {}", record.EventID & 0xFFFF),
                    event_id: record.EventID & 0xFFFF,
                });
            }

            offset += record.Length as usize;

            if entries.len() >= RING_SIZE {
                done = true;
                break;
            }
        }
    }

    unsafe {
        let _ = CloseEventLog(handle);
    }
    (entries, new_max_record)
}

pub fn spawn(
    channels: Vec<String>,
    poll_interval_secs: u64,
    trigger: super::TriggerTx,
) -> (SharedEntries, watch::Sender<()>) {
    let shared: SharedEntries = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
        // Per-channel cursor: highest record number delivered so far.
        // Initialised to 0 on first poll; after that only new records are returned.
        let mut cursors: HashMap<String, u32> = HashMap::new();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let channels_clone = channels.clone();
                    let cursors_in = cursors.clone();

                    let polled = tokio::task::spawn_blocking(move || {
                        let mut all = VecDeque::new();
                        let mut c = cursors_in;
                        for channel in &channels_clone {
                            // Absent cursor == first poll for this channel → prime
                            // (seed the high-water mark, deliver nothing).
                            let known = c.get(channel.as_str()).copied();
                            let last = known.unwrap_or(0);
                            let prime = known.is_none();
                            let (entries, new_last) = read_channel_since(channel, last, prime);
                            // Always record the cursor on a prime (even if it didn't
                            // advance) so the channel isn't re-primed every poll.
                            if prime || new_last != last {
                                c.insert(channel.clone(), new_last);
                            }
                            for e in entries {
                                if all.len() >= RING_SIZE { break; }
                                all.push_back(e);
                            }
                            if all.len() >= RING_SIZE { break; }
                        }
                        (all, c)
                    })
                    .await;

                    // Preserve the existing cursors on a join failure (a panic in the
                    // raw-buffer parse). `unwrap_or_default()` here would reset every
                    // channel's high-water mark to 0, re-delivering historical Errors
                    // and re-firing the reactive trigger on every subsequent poll.
                    let (new_entries, updated_cursors) = match polled {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("event-log poll task failed, keeping cursors: {e}");
                            continue;
                        }
                    };

                    cursors = updated_cursors;
                    let count = new_entries.len();
                    // A fresh Error wakes the decision loop immediately.
                    // Warnings deliberately don't trigger (Windows emits them
                    // near-continuously — the scheduled sweep still sees them);
                    // reacting to each would burn AI calls on noise.
                    let actionable = new_entries.iter().any(|e| e.level == "Error");
                    if let Ok(mut guard) = shared_clone.lock() {
                        // Accumulate into a rolling buffer the decision loop
                        // DRAINS (one-shot delivery). Replacing wholesale would
                        // let a quieter follow-up poll wipe entries before the
                        // (debounced) reactive cycle ever reads them.
                        for e in new_entries {
                            if guard.len() >= BUFFER_CAP {
                                guard.pop_front();
                            }
                            guard.push_back(e);
                        }
                    }
                    if actionable {
                        let _ = trigger.try_send(());
                    }
                    info!(entries = count, "Event log polled");
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    (shared, shutdown_tx)
}

/// Take (and clear) everything collected since the last drain — each entry is
/// delivered to the decision loop exactly once, like the file-watch buffer.
pub fn drain(shared: &SharedEntries) -> Vec<EventLogEntry> {
    shared
        .lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}
