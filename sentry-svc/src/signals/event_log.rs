use crate::models::EventLogEntry;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
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

fn read_channel(channel: &str) -> Vec<EventLogEntry> {
    let channel_w = wide(channel);
    let handle = match unsafe { OpenEventLogW(PCWSTR::null(), PCWSTR(channel_w.as_ptr())) } {
        Ok(h) => h,
        Err(e) => {
            warn!("Failed to open event log channel {channel}: {e}");
            return vec![];
        }
    };

    let mut entries = Vec::new();
    let mut buf = vec![0u8; 65536];

    loop {
        let mut bytes_read: u32 = 0;
        let mut min_bytes_needed: u32 = 0;

        let ok = unsafe {
            ReadEventLogW(
                handle,
                SEQUENTIAL_BACKWARDS,
                0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut bytes_read,
                &mut min_bytes_needed,
            )
        };

        if ok.is_err() {
            break;
        }

        let mut offset = 0usize;
        while offset < bytes_read as usize {
            let record = unsafe { &*(buf.as_ptr().add(offset) as *const EVENTLOGRECORD) };

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

            if record.Length == 0 {
                break;
            }
            offset += record.Length as usize;
        }

        if entries.len() >= RING_SIZE {
            break;
        }
    }

    unsafe {
        let _ = CloseEventLog(handle);
    }
    entries.truncate(RING_SIZE);
    entries
}

pub fn spawn(channels: Vec<String>, poll_interval_secs: u64) -> (SharedEntries, watch::Sender<()>) {
    let shared: SharedEntries = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let channels_clone = channels.clone();
                    let new_entries = tokio::task::spawn_blocking(move || {
                        let mut all = VecDeque::new();
                        for channel in &channels_clone {
                            for e in read_channel(channel) {
                                if all.len() >= RING_SIZE { break; }
                                all.push_back(e);
                            }
                            if all.len() >= RING_SIZE { break; }
                        }
                        all
                    }).await.unwrap_or_default();

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
