use crate::models::FileChange;
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};

const RING_SIZE: usize = 50;
const MAX_READ_BYTES: u64 = 65_536; // 64 KB — only read files below this size

const TEXT_EXTENSIONS: &[&str] = &[
    "log", "txt", "csv", "json", "xml", "ini", "cfg", "conf",
    "err", "out", "trace", "debug", "warn", "error", "info",
];

pub type SharedChanges = Arc<Mutex<VecDeque<FileChange>>>;

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
    // Only keep events that found something interesting
    if event.error_snippets.is_empty() && event.severity == "INFO" {
        None
    } else {
        Some(event)
    }
}

pub fn spawn(directories: Vec<String>) -> (SharedChanges, watch::Sender<()>) {
    let shared: SharedChanges = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    let (shutdown_tx, _shutdown_rx) = watch::channel(());

    let dirs: Vec<PathBuf> = directories
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    if dirs.is_empty() {
        warn!("No watchable log directories found");
        return (shared, shutdown_tx);
    }

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to create file watcher: {e}");
            return (shared, shutdown_tx);
        }
    };

    for dir in &dirs {
        if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
            warn!("Failed to watch {}: {e}", dir.display());
        } else {
            info!("Watching directory: {}", dir.display());
        }
    }

    std::thread::spawn(move || {
        let _watcher = watcher;
        for result in rx {
            match result {
                Ok(event) => {
                    let kind = match event.kind {
                        EventKind::Create(_) => "created",
                        EventKind::Modify(_) => "modified",
                        _ => continue,
                    };

                    for path in event.paths {
                        let size_bytes =
                            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

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
                Err(e) => warn!("File watch error: {e}"),
            }
        }
    });

    (shared, shutdown_tx)
}

pub fn drain(shared: &SharedChanges) -> Vec<FileChange> {
    shared
        .lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}
