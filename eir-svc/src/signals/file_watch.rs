use crate::models::FileChange;
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

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
    if size_bytes == 0 || size_bytes > MAX_READ_BYTES {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    // Bound the ACTUAL read, not just the pre-read metadata size: the file may have
    // grown between the metadata() sample and here (an actively-written log), and
    // read_to_string would otherwise pull the whole current file into memory.
    let content = {
        use std::io::Read;
        let file = std::fs::File::open(path).ok()?;
        let mut buf = String::new();
        // +1 so a file that grew to exactly the cap is detectable, but we still only
        // ever hold at most MAX_READ_BYTES+1 bytes.
        file.take(MAX_READ_BYTES + 1)
            .read_to_string(&mut buf)
            .ok()?;
        if buf.len() as u64 > MAX_READ_BYTES {
            return None;
        }
        buf
    };
    let event = super::log_parser::parse(path, &content);
    if event.error_snippets.is_empty() && event.severity == "INFO" {
        None
    } else {
        Some(event)
    }
}

// ── Directory discovery ───────────────────────────────────────────────────────

/// Scan standard Windows log locations and return only the directories that
/// contain log files modified within the last `DISCOVERY_WINDOW_DAYS` days.
///
/// Always includes any `extra` paths from `config.toml` that exist on disk,
/// regardless of age. Designed to run via `tokio::task::spawn_blocking`.
pub fn discover_watch_dirs(extra: &[String]) -> Vec<PathBuf> {
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

    // Config extras are always included if the path exists on this machine
    for path in extra {
        let p = PathBuf::from(path);
        if p.exists() {
            result.insert(p);
        }
    }

    let mut dirs: Vec<PathBuf> = result.into_iter().collect();
    dirs.sort();
    dirs
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

    if directories.is_empty() {
        warn!("No log directories discovered — file watcher inactive");
        return (shared, shutdown_tx, dir_tx);
    }

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
                if watched_dirs.contains(&new_dir) || !new_dir.exists() {
                    continue;
                }
                match watcher.watch(&new_dir, RecursiveMode::Recursive) {
                    Ok(()) => {
                        info!("Now watching: {}", new_dir.display());
                        watched_dirs.insert(new_dir);
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
                        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        let log_event = try_parse_log(&path, size_bytes);
                        // Only KEEP a change that carries a parsed log event. The watch
                        // trees (%TEMP%, %LOCALAPPDATA%, …) churn constantly with
                        // browser-cache/temp writes; pushing those non-log changes into
                        // the small ring evicts genuine error-log events before the
                        // decision loop drains them, so a fired trigger would arrive with
                        // no supporting evidence. Non-log noise is simply dropped.
                        let Some(log_event) = log_event else {
                            continue;
                        };
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
