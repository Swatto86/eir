//! Reactive error-signal collector: the Windows Event Log or Linux journald, polled on
//! `monitoring.event_log_poll_interval_secs` and drained into the decision loop exactly
//! once per entry. The collector body is platform-specific; `SharedEntries` and
//! `drain()` are shared and unchanged from before the Linux port.

use crate::models::EventLogEntry;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::spawn;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{failed_units_context, spawn};

pub type SharedEntries = Arc<Mutex<VecDeque<EventLogEntry>>>;

/// Take (and clear) everything collected since the last drain — each entry is
/// delivered to the decision loop exactly once, like the file-watch buffer.
pub fn drain(shared: &SharedEntries) -> Vec<EventLogEntry> {
    shared
        .lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}
