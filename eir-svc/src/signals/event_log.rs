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
/// On first call pass `last_record = 0` to prime the cursor without returning entries,
/// or any prior max to receive only genuinely new events.
fn read_channel_since(channel: &str, last_record: u32) -> (Vec<EventLogEntry>, u32) {
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

pub fn spawn(channels: Vec<String>, poll_interval_secs: u64) -> (SharedEntries, watch::Sender<()>) {
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

                    let (new_entries, updated_cursors) = tokio::task::spawn_blocking(move || {
                        let mut all = VecDeque::new();
                        let mut c = cursors_in;
                        for channel in &channels_clone {
                            let last = *c.get(channel.as_str()).unwrap_or(&0);
                            let (entries, new_last) = read_channel_since(channel, last);
                            if new_last > last {
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
                    .await
                    .unwrap_or_default();

                    cursors = updated_cursors;
                    let count = new_entries.len();
                    if let Ok(mut guard) = shared_clone.lock() {
                        *guard = new_entries;
                    }
                    info!(entries = count, "Event log polled");
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    (shared, shutdown_tx)
}

pub fn snapshot(shared: &SharedEntries) -> Vec<EventLogEntry> {
    shared
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default()
}
