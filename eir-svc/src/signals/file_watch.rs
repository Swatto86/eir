use crate::models::FileChange;
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{ImpersonateLoggedOnUser, RevertToSelf},
    System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
};

const RING_SIZE: usize = 50;
const MAX_READ_BYTES: u64 = 65_536;
const DISCOVERY_WINDOW_DAYS: u64 = 30;

pub const TEXT_EXTENSIONS: &[&str] = &[
    "log", "txt", "csv", "json", "xml", "ini", "cfg", "conf", "err", "out", "trace", "debug",
    "warn", "error", "info",
];

pub type SharedChanges = Arc<Mutex<VecDeque<FileChange>>>;
/// Send new directories to the running watcher thread after startup.
pub type DirUpdateSender = std::sync::mpsc::Sender<PathBuf>;
/// Dropping this handle signals the watcher thread to exit.
pub type ShutdownHandle = std::sync::mpsc::SyncSender<()>;

// ── Log parsing ───────────────────────────────────────────────────────────────

fn try_parse_log(path: &Path, size_bytes: u64) -> Option<crate::models::LogEvent> {
    if size_bytes == 0 {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    // Read at most the LAST MAX_READ_BYTES. A rolling log grows past 64KB and its
    // newest (most relevant) lines are at the END — skipping any file over the cap
    // (as before) meant that once a busy log crossed 64KB Eir went permanently blind
    // to the errors still being written to it.
    let content = read_tail(path, MAX_READ_BYTES)?;
    let event = super::log_parser::parse(path, &content);
    if event.error_snippets.is_empty() && event.severity == "INFO" {
        None
    } else {
        Some(event)
    }
}

struct ActiveUserImpersonation(HANDLE);

impl ActiveUserImpersonation {
    fn new() -> Option<Self> {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == u32::MAX {
            return None;
        }
        let mut token = HANDLE::default();
        unsafe {
            WTSQueryUserToken(session, &mut token).ok()?;
            if let Err(e) = ImpersonateLoggedOnUser(token) {
                let _ = CloseHandle(token);
                warn!("Cannot impersonate active user for configured log path: {e}");
                return None;
            }
        }
        Some(Self(token))
    }
}

impl Drop for ActiveUserImpersonation {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
            let _ = CloseHandle(self.0);
        }
    }
}

fn parse_path(path: &Path, as_active_user: bool) -> Option<(u64, crate::models::LogEvent)> {
    let _user = if as_active_user {
        Some(ActiveUserImpersonation::new()?)
    } else {
        None
    };
    let size = std::fs::metadata(path).ok()?.len();
    try_parse_log(path, size).map(|event| (size, event))
}

/// Read up to the last `max_bytes` of a file as (lossy) UTF-8. If the file is
/// larger, seeks to the tail and drops the first — likely partial — line so the
/// parser never keys off a truncated leading record.
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Skip the partial first line left by seeking into the middle of a record.
        if let Some(nl) = text.find('\n') {
            return Some(text[nl + 1..].to_string());
        }
    }
    Some(text)
}

// ── Directory discovery ───────────────────────────────────────────────────────

/// Scan standard Windows log locations and return only the directories that
/// contain log files modified within the last `DISCOVERY_WINDOW_DAYS` days.
///
/// Always includes any `extra` paths from `config.toml` that exist on disk,
/// regardless of age. Designed to run via `tokio::task::spawn_blocking`.
pub fn discover_watch_dirs(extra: &[String]) -> Vec<PathBuf> {
    let Some(_user) = ActiveUserImpersonation::new() else {
        warn!("Log directory discovery deferred: active user token unavailable");
        return Vec::new();
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(DISCOVERY_WINDOW_DAYS * 86400))
        .unwrap_or(UNIX_EPOCH);

    // Roots to scan: fixed system paths + env-var-based user paths
    let auto_roots: Vec<PathBuf> = [
        "C:\\Windows\\Logs",
        "C:\\Windows\\Temp",
        "C:\\Temp",
        "C:\\Logs",
    ]
    .iter()
    .map(PathBuf::from)
    .chain(
        ["LOCALAPPDATA", "APPDATA", "PROGRAMDATA", "TEMP", "TMP"]
            .iter()
            .filter_map(|v| std::env::var(v).ok().map(PathBuf::from)),
    )
    .collect();

    let mut result: HashSet<PathBuf> = HashSet::new();

    for root in &auto_roots {
        if !root.exists() {
            continue;
        }

        // If the root itself has recent log files at depth ≤ 1, watch it directly
        if has_recent_log_files(root, cutoff, 1) {
            result.insert(root.clone());
        }

        // Scan one level of subdirectories; add those with recent log activity
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() && has_recent_log_files(&sub, cutoff, 2) {
                    result.insert(sub);
                }
            }
        }
    }

    for path in configured_watch_dirs_current_user(extra) {
        result.insert(path);
    }

    let mut dirs: Vec<PathBuf> = result.into_iter().collect();
    dirs.sort();
    dirs
}

/// Resolve configured roots while already impersonating the active desktop user.
fn configured_watch_dirs_current_user(extra: &[String]) -> Vec<PathBuf> {
    let mut dirs = HashSet::new();
    for path in extra {
        let canonical = std::fs::canonicalize(path);
        match canonical {
            Ok(path) if path.is_dir() && canonical_is_local_drive(&path) => {
                dirs.insert(path);
            }
            Ok(_) => warn!("Configured log directory did not resolve to a local drive: {path}"),
            Err(e) => {
                warn!("Configured log directory is unavailable to the active user ({path}): {e}")
            }
        }
    }
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort();
    dirs
}

fn canonical_is_local_drive(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) && matches!(components.next(), Some(Component::RootDir))
}

/// Returns true if `dir` contains at least one recognised text-extension file
/// modified after `cutoff`, looking no deeper than `max_depth` levels.
fn has_recent_log_files(dir: &Path, cutoff: SystemTime, max_depth: usize) -> bool {
    walkdir::WalkDir::new(dir)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| {
            let ext = e
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            TEXT_EXTENSIONS.contains(&ext.as_str())
                && e.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| t > cutoff)
                    .unwrap_or(false)
        })
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Start the file-watch background thread watching `directories`.
///
/// Returns a `ShutdownHandle` — dropping it signals the thread to exit — and a
/// `DirUpdateSender` for adding new directories at runtime.
pub fn spawn(
    directories: Vec<PathBuf>,
    trigger: super::TriggerTx,
) -> (SharedChanges, ShutdownHandle, DirUpdateSender) {
    let shared: SharedChanges = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    // SyncSender with cap 0: never blocks on send; drops when caller drops the handle,
    // causing try_recv in the thread to return Disconnected → thread exits.
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let (dir_tx, dir_rx) = std::sync::mpsc::channel::<PathBuf>();

    let (event_tx, event_rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = match RecommendedWatcher::new(event_tx, Config::default()) {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to create file watcher: {e}");
            return (shared, shutdown_tx, dir_tx);
        }
    };

    let mut watched: HashSet<PathBuf> = HashSet::new();
    for dir in &directories {
        let Some(_user) = ActiveUserImpersonation::new() else {
            warn!(
                "Cannot watch directory without active-user token: {}",
                dir.display()
            );
            continue;
        };
        match watcher.watch(dir, RecursiveMode::Recursive) {
            Ok(()) => {
                watched.insert(dir.clone());
            }
            Err(e) => warn!("Cannot watch {}: {e}", dir.display()),
        }
    }
    info!(dirs = watched.len(), "File watcher started");

    std::thread::spawn(move || {
        let mut watcher = watcher;
        let mut watched_dirs = watched;

        while let Err(std::sync::mpsc::TryRecvError::Empty) = shutdown_rx.try_recv() {
            // Check for directories added by the main loop's re-discovery
            while let Ok(new_dir) = dir_rx.try_recv() {
                let Some(_user) = ActiveUserImpersonation::new() else {
                    warn!(
                        "Cannot re-watch directory without active-user token: {}",
                        new_dir.display()
                    );
                    continue;
                };
                if !new_dir.exists() {
                    continue;
                }
                // Re-issue watch() even for a dir we already track. If a watched
                // directory was deleted and recreated, the OS handle behind the old
                // ReadDirectoryChangesW watch is dead and notify silently stops
                // delivering its events — with no error to tell us which watch died.
                // notify re-arms an already-watched path harmlessly, so re-watching on
                // each rediscovery repairs a stale watch without a service restart.
                let already = watched_dirs.contains(&new_dir);
                match watcher.watch(&new_dir, RecursiveMode::Recursive) {
                    Ok(()) => {
                        if !already {
                            info!("Now watching: {}", new_dir.display());
                            watched_dirs.insert(new_dir);
                        }
                    }
                    Err(e) => warn!("Cannot watch {}: {e}", new_dir.display()),
                }
            }

            // Wait briefly for a file-system event; loop back to check dir_rx if none
            match event_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(event)) => {
                    let kind = match event.kind {
                        EventKind::Create(_) => "created",
                        EventKind::Modify(_) => "modified",
                        _ => continue,
                    };
                    for path in event.paths {
                        let Some((size_bytes, log_event)) = parse_path(&path, true) else {
                            continue;
                        };
                        // Only KEEP a change that carries a parsed log event. The watch
                        // trees (%TEMP%, %LOCALAPPDATA%, …) churn constantly with
                        // browser-cache/temp writes; pushing those non-log changes into
                        // the small ring evicts genuine error-log events before the
                        // decision loop drains them, so a fired trigger would arrive with
                        // no supporting evidence. Non-log noise is simply dropped.
                        // Error-bearing log writes are actionable — wake the
                        // decision loop (try_send is fine off the runtime).
                        let actionable = log_event.is_actionable();
                        let change = FileChange {
                            path,
                            kind: kind.to_string(),
                            size_bytes,
                            timestamp: Utc::now(),
                            log_event: Some(log_event),
                        };
                        if let Ok(mut guard) = shared_clone.lock() {
                            if guard.len() >= RING_SIZE {
                                guard.pop_front();
                            }
                            guard.push_back(change);
                        }
                        if actionable {
                            let _ = trigger.try_send(());
                        }
                    }
                }
                Ok(Err(e)) => warn!("File watch error: {e}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    (shared, shutdown_tx, dir_tx)
}

pub fn drain(shared: &SharedChanges) -> Vec<FileChange> {
    shared
        .lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tail_keeps_newest_content_and_drops_partial_first_line() {
        // A file larger than the window: the newest line must survive and the far-older
        // head line must not — the bug this replaced skipped >64KB files entirely.
        let path = std::env::temp_dir().join(format!("eir-readtail-{}.log", std::process::id()));
        let filler = "x".repeat(MAX_READ_BYTES as usize);
        std::fs::write(
            &path,
            format!("HEAD-should-be-gone\n{filler}\nTAIL-ERROR-marker\n"),
        )
        .unwrap();

        let tail = read_tail(&path, MAX_READ_BYTES).expect("tail");
        let _ = std::fs::remove_file(&path);

        assert!(tail.len() as u64 <= MAX_READ_BYTES);
        assert!(tail.contains("TAIL-ERROR-marker"));
        assert!(!tail.contains("HEAD-should-be-gone"));
    }

    #[test]
    fn read_tail_reads_a_small_file_whole() {
        let path =
            std::env::temp_dir().join(format!("eir-readtail-small-{}.log", std::process::id()));
        std::fs::write(&path, "line1\nERROR boom\n").unwrap();
        let tail = read_tail(&path, MAX_READ_BYTES).expect("tail");
        let _ = std::fs::remove_file(&path);
        assert!(tail.contains("line1"));
        assert!(tail.contains("ERROR boom"));
    }
}
