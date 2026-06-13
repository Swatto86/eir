use crate::models::FileChange;
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
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

// ── Log parsing ───────────────────────────────────────────────────────────────

fn try_parse_log(path: &Path, size_bytes: u64) -> Option<crate::models::LogEvent> {
    if size_bytes == 0 || size_bytes > MAX_READ_BYTES {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
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
/// Returns a `DirUpdateSender` that the caller can use to add new directories
/// at runtime (e.g. from periodic re-discovery in the main loop).
pub fn spawn(directories: Vec<PathBuf>) -> (SharedChanges, watch::Sender<()>, DirUpdateSender) {
    let shared: SharedChanges = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    let (shutdown_tx, _shutdown_rx) = watch::channel(());
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

        loop {
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
                        let change = FileChange {
                            path,
                            kind: kind.to_string(),
                            size_bytes,
                            timestamp: Utc::now(),
                            log_event,
                        };
                        if let Ok(mut guard) = shared_clone.lock() {
                            if guard.len() >= RING_SIZE {
                                guard.pop_front();
                            }
                            guard.push_back(change);
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
