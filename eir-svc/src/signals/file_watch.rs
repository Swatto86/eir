use crate::models::FileChange;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use super::log_memory::LogMemory;
#[cfg(windows)]
use crate::executor::logs::{checked_local_path, root_too_broad};
#[cfg(windows)]
use crate::session::{active_user_session_id, system_drive_root, user_profile_dir_for_token};
#[cfg(windows)]
use chrono::Utc;
#[cfg(windows)]
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::path::{Component, Prefix};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::time::UNIX_EPOCH;
#[cfg(windows)]
use std::time::{Instant, SystemTime};
#[cfg(windows)]
use tracing::info;
#[cfg(windows)]
use tracing::warn;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{ImpersonateLoggedOnUser, RevertToSelf},
    Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::RemoteDesktop::WTSQueryUserToken,
};

#[cfg(windows)]
const RING_SIZE: usize = 50;
#[cfg(windows)]
const EVENT_QUEUE_SIZE: usize = 256;
#[cfg(windows)]
const MAX_READ_BYTES: u64 = 65_536;
#[cfg(windows)]
const DISCOVERY_WINDOW_DAYS: u64 = 30;
#[cfg(windows)]
const DISCOVERY_MAX: Duration = Duration::from_secs(30);
#[cfg(windows)]
const MAX_AUTO_WATCH_DIRS: usize = 96;
#[cfg(windows)]
const MAX_DISCOVERY_ENTRIES_PER_ROOT: usize = 4096;
/// Shortest gap between two "events were dropped" warnings.
#[cfg(windows)]
const DROP_WARNING_EVERY: Duration = Duration::from_secs(10 * 60);

/// Extensions read as logs. Structured state files (.json, .xml, .ini, .cfg, .conf,
/// .csv) are deliberately absent: apps rewrite them constantly and they are full of
/// keys such as `"errors": 0` or `"crashed": false`, which made them the watcher's
/// largest source of false alarms without once contributing a real finding.
#[cfg(windows)]
pub const TEXT_EXTENSIONS: &[&str] = &[
    "log", "txt", "err", "out", "trace", "debug", "warn", "error", "info",
];

pub type SharedChanges = Arc<Mutex<VecDeque<FileChange>>>;
/// Send the complete desired directory set to the running watcher thread.
pub type DirUpdateSender = std::sync::mpsc::Sender<Vec<PathBuf>>;
/// Dropping this handle signals the watcher thread to exit.
pub type ShutdownHandle = std::sync::mpsc::SyncSender<()>;

// ── Log parsing ───────────────────────────────────────────────────────────────

/// Parse what was written to a log since the watcher last read it. Only new bytes are
/// read (at most MAX_READ_BYTES, taken from the end, so a busy log past 64KB is never
/// skipped), and only error or warning lines not already reported for this file
/// count, so a line an app keeps writing is reported once rather than on every write.
#[cfg(windows)]
fn try_parse_log(
    path: &Path,
    size_bytes: u64,
    memory: &mut LogMemory,
) -> Option<crate::models::LogEvent> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let now = Instant::now();
    let start = memory.read_start(path, size_bytes, MAX_READ_BYTES, now)?;
    let (content, read_to) = read_chunk(path, start.offset, MAX_READ_BYTES, start.mid_line)?;
    memory.mark_read(path, read_to, now);
    let event = super::log_parser::parse(path, &content, |line| {
        memory.first_sighting(path, line, now)
    });
    if event.error_snippets.is_empty() && event.severity == "INFO" {
        None
    } else {
        Some(event)
    }
}

/// Paths the watcher never reads. Eir's own folders: its data and the scratch folders
/// its AI runs work in, which hold the prompt and the model's reply — watching them
/// made Eir analyse its own analyses. And the desktop user's Temp folder, where build
/// tools and installers leave short-lived logs full of expected errors; it was a
/// constant source of alarms and never of a real finding.
#[cfg(windows)]
fn is_ignored_dir(dir: &Path) -> bool {
    let parts: Vec<String> = dir
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    parts
        .iter()
        .any(|part| part == "eir" || part == "co.swatto.eir" || part.starts_with("eir-"))
        || parts
            .windows(3)
            .any(|w| w[0] == "appdata" && w[1] == "local" && w[2] == "temp")
}

/// Fold a newer change to the same file into the one still waiting to be drained, so
/// a busy log becomes one entry per analysis rather than dozens.
#[cfg(windows)]
fn merge_change(into: &mut FileChange, newer: FileChange) {
    into.kind = newer.kind;
    into.size_bytes = newer.size_bytes;
    into.timestamp = newer.timestamp;
    match (&mut into.log_event, newer.log_event) {
        (Some(old), Some(new)) => {
            if super::log_parser::severity_rank(&new.severity)
                > super::log_parser::severity_rank(&old.severity)
            {
                old.severity = new.severity;
            }
            let room = super::log_parser::MAX_SNIPPETS.saturating_sub(old.error_snippets.len());
            old.error_snippets
                .extend(new.error_snippets.into_iter().take(room));
            if old.content_excerpt.is_empty() {
                old.content_excerpt = new.content_excerpt;
            }
        }
        (slot @ None, new) => *slot = new,
        (Some(_), None) => {}
    }
}

#[cfg(windows)]
struct ActiveUserImpersonation(HANDLE);

#[cfg(windows)]
impl ActiveUserImpersonation {
    fn new() -> Option<Self> {
        let session = active_user_session_id()?;
        let mut token = HANDLE::default();
        unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
        if let Err(e) = unsafe { ImpersonateLoggedOnUser(token) } {
            unsafe {
                let _ = CloseHandle(token);
            }
            warn!("Cannot impersonate active user for configured log path: {e}");
            return None;
        }
        Some(Self(token))
    }

    fn profile_dir(&self) -> Option<PathBuf> {
        user_profile_dir_for_token(self.0)
    }
}

#[cfg(windows)]
impl Drop for ActiveUserImpersonation {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct WatchPathGuard(Vec<HANDLE>);

#[cfg(windows)]
impl WatchPathGuard {
    fn open(path: &Path) -> Result<Self, String> {
        let mut guard = Self(Vec::new());
        let mut current = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix)
                    if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) =>
                {
                    current.push(component.as_os_str());
                }
                Component::RootDir => current.push(component.as_os_str()),
                Component::Normal(_) => {
                    current.push(component.as_os_str());
                    let wide: Vec<u16> = current
                        .as_os_str()
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    let handle = unsafe {
                        CreateFileW(
                            PCWSTR(wide.as_ptr()),
                            FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                            None,
                            OPEN_EXISTING,
                            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                            HANDLE::default(),
                        )
                    }
                    .map_err(|e| format!("Cannot lock '{}': {e}", current.display()))?;
                    guard.0.push(handle);
                    let mut info = BY_HANDLE_FILE_INFORMATION::default();
                    unsafe { GetFileInformationByHandle(handle, &mut info) }
                        .map_err(|e| format!("Cannot inspect '{}': {e}", current.display()))?;
                    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
                    {
                        return Err(format!(
                            "Refusing '{}' because a path component is not a plain directory",
                            path.display()
                        ));
                    }
                }
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "Refusing '{}' because it is not a plain local path",
                        path.display()
                    ));
                }
            }
        }
        if guard.0.is_empty() {
            return Err(format!("Refusing broad watch path '{}'", path.display()));
        }
        Ok(guard)
    }
}

#[cfg(windows)]
impl Drop for WatchPathGuard {
    fn drop(&mut self) {
        for handle in self.0.drain(..).rev() {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

#[cfg(windows)]
fn arm_watch(watcher: &mut RecommendedWatcher, path: &Path) -> Result<(), String> {
    let _user = ActiveUserImpersonation::new()
        .ok_or_else(|| "active-user token is unavailable".to_string())?;
    // notify opens the real directory handle on its own SYSTEM worker thread. Hold
    // every path component open without delete-sharing until that open is acknowledged,
    // so a user-controlled directory cannot be swapped for a reparse point in between.
    let _path_guard = WatchPathGuard::open(path)?;
    watcher
        .watch(path, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn replacement_watch_dirs(directories: &[PathBuf]) -> HashSet<PathBuf> {
    directories.iter().cloned().collect()
}

/// Whether the watched directory set actually changed (a fast-user-switch), as
/// opposed to a routine re-arm of the same roots. Only a real change justifies
/// dropping buffered, un-drained events.
#[cfg(windows)]
fn watched_set_changed(previous: &HashSet<PathBuf>, next: &HashSet<PathBuf>) -> bool {
    previous != next
}

#[cfg(windows)]
fn parse_path(
    path: &Path,
    as_active_user: bool,
    memory: &mut LogMemory,
) -> Option<(u64, crate::models::LogEvent)> {
    let _user = if as_active_user {
        Some(ActiveUserImpersonation::new()?)
    } else {
        None
    };
    let size = std::fs::metadata(path).ok()?.len();
    try_parse_log(path, size, memory).map(|event| (size, event))
}

/// Read up to `max_bytes` of a file from byte `start` as (lossy) UTF-8, returning the
/// text and the offset it was consumed up to. A line still being written (no newline
/// yet) is left for the next read so it is never parsed in two halves; `mid_line`
/// drops a partial first line left by starting inside a record.
#[cfg(windows)]
fn read_chunk(path: &Path, start: u64, max_bytes: u64, mid_line: bool) -> Option<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    let end = match buf.iter().rposition(|&byte| byte == b'\n') {
        Some(newline) => newline + 1,
        None => buf.len(),
    };
    let mut text = &buf[..end];
    if mid_line {
        if let Some(newline) = text.iter().position(|&byte| byte == b'\n') {
            text = &text[newline + 1..];
        }
    }
    let consumed = u64::try_from(end).ok()?;
    Some((String::from_utf8_lossy(text).into_owned(), start + consumed))
}

// ── Directory discovery ───────────────────────────────────────────────────────

/// Scan standard Windows log locations and return only the directories that
/// contain log files modified within the last `DISCOVERY_WINDOW_DAYS` days.
///
/// Always includes any `extra` paths from `config.toml` that exist on disk,
/// regardless of age. Designed to run via `tokio::task::spawn_blocking`.
#[cfg(windows)]
pub fn discover_watch_dirs(extra: &[String]) -> Option<Vec<PathBuf>> {
    let Some(user) = ActiveUserImpersonation::new() else {
        warn!("Log directory discovery deferred: active user token unavailable");
        return None;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(DISCOVERY_WINDOW_DAYS * 86400))
        .unwrap_or(UNIX_EPOCH);
    let deadline = Instant::now() + DISCOVERY_MAX;

    let system_drive = system_drive_root();
    let windows = system_drive.join("Windows");
    let mut auto_roots = vec![
        windows.join("Logs"),
        windows.join("Temp"),
        system_drive.join("Temp"),
        system_drive.join("Logs"),
        std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| system_drive.join("ProgramData")),
    ];
    if let Some(profile) = user.profile_dir() {
        auto_roots.extend(profile_watch_roots(&profile));
    }

    let mut result: HashSet<PathBuf> = HashSet::new();

    for root in auto_roots {
        if result.len() >= MAX_AUTO_WATCH_DIRS || Instant::now() >= deadline {
            break;
        }
        let Ok(Some(root)) = checked_local_path(&root) else {
            continue;
        };
        if !root.is_dir() || root_too_broad(&root.to_string_lossy()) {
            continue;
        }

        // If the root itself has recent log files at depth ≤ 1, watch it directly
        if has_recent_log_files(&root, cutoff, 1, deadline) {
            result.insert(root.clone());
        }

        // Scan one level of subdirectories; add those with recent log activity
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if result.len() >= MAX_AUTO_WATCH_DIRS || Instant::now() >= deadline {
                    break;
                }
                let sub = entry.path();
                if sub.is_dir()
                    && !is_ignored_dir(&sub)
                    && has_recent_log_files(&sub, cutoff, 2, deadline)
                {
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
    Some(dirs)
}

#[cfg(windows)]
fn profile_watch_roots(profile: &Path) -> [PathBuf; 2] {
    [
        profile.join("AppData\\Local"),
        profile.join("AppData\\Roaming"),
    ]
}

/// Resolve configured roots while already impersonating the active desktop user.
#[cfg(windows)]
fn configured_watch_dirs_current_user(extra: &[String]) -> Vec<PathBuf> {
    let mut dirs = HashSet::new();
    for path in extra {
        match checked_local_path(Path::new(path)) {
            Ok(Some(canonical))
                if canonical.is_dir() && !root_too_broad(&canonical.to_string_lossy()) =>
            {
                dirs.insert(canonical);
            }
            Ok(Some(_)) => warn!("Configured log directory is too broad or unsafe: {path}"),
            Ok(None) => warn!("Configured log directory does not exist: {path}"),
            Err(e) => warn!("Configured log directory is unsafe or unavailable ({path}): {e}"),
        }
    }
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort();
    dirs
}

/// Returns true if `dir` contains at least one recognised text-extension file
/// modified after `cutoff`, looking no deeper than `max_depth` levels.
#[cfg(windows)]
fn has_recent_log_files(
    dir: &Path,
    cutoff: SystemTime,
    max_depth: usize,
    deadline: Instant,
) -> bool {
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .take(MAX_DISCOVERY_ENTRIES_PER_ROOT)
    {
        if Instant::now() >= deadline {
            return false;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();
        if TEXT_EXTENSIONS.contains(&ext.as_str())
            && entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .is_some_and(|modified| modified > cutoff)
        {
            return true;
        }
    }
    false
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// A file the watcher reads: a log extension outside the ignored folders. Path-only,
/// so it is cheap enough to run on every raw file-system event.
#[cfg(windows)]
fn is_log_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| TEXT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        && path.parent().is_some_and(|dir| !is_ignored_dir(dir))
}

/// Whether a raw event is worth queueing: a creation or change of a log file. The
/// watched trees churn constantly with cache and temp writes; queueing all of them
/// overflowed the queue several hundred thousand times on a normal desktop and dropped
/// real log events with the noise.
#[cfg(windows)]
fn worth_queueing(event: &notify::Result<Event>) -> bool {
    match event {
        Ok(event) => {
            matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                && event.paths.iter().any(|path| is_log_candidate(path))
        }
        Err(_) => true,
    }
}

#[cfg(windows)]
fn create_watcher(
    event_tx: std::sync::mpsc::SyncSender<notify::Result<Event>>,
    dropped: Arc<AtomicU64>,
) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(
        move |event| {
            if worth_queueing(&event)
                && matches!(
                    event_tx.try_send(event),
                    Err(std::sync::mpsc::TrySendError::Full(_))
                )
            {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        },
        Config::default(),
    )
}

/// Start the file-watch background thread watching `directories`.
///
/// Returns a `ShutdownHandle` — dropping it signals the thread to exit — and a
/// `DirUpdateSender` for replacing the complete directory set at runtime.
#[cfg(windows)]
pub fn spawn(
    directories: Vec<PathBuf>,
    trigger: super::TriggerTx,
) -> (SharedChanges, ShutdownHandle, DirUpdateSender) {
    let shared: SharedChanges = Arc::new(Mutex::new(VecDeque::new()));
    let shared_clone = shared.clone();
    // SyncSender with cap 0: never blocks on send; drops when caller drops the handle,
    // causing try_recv in the thread to return Disconnected → thread exits.
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let (dir_tx, dir_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();

    let (event_tx, event_rx) =
        std::sync::mpsc::sync_channel::<notify::Result<Event>>(EVENT_QUEUE_SIZE);
    let events_dropped = Arc::new(AtomicU64::new(0));

    let mut watcher = match create_watcher(event_tx.clone(), events_dropped.clone()) {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to create file watcher: {e}");
            return (shared, shutdown_tx, dir_tx);
        }
    };

    let mut watched: HashSet<PathBuf> = HashSet::new();
    for dir in &directories {
        match arm_watch(&mut watcher, dir) {
            Ok(()) => {
                watched.insert(dir.clone());
            }
            Err(e) => warn!("Cannot watch {}: {e}", dir.display()),
        }
    }
    info!(dirs = watched.len(), "File watcher started");

    std::thread::spawn(move || {
        let mut watcher = Some(watcher);
        let mut watched_dirs = watched;
        let mut memory = LogMemory::default();
        let mut last_drop_warning: Option<Instant> = None;

        while let Err(std::sync::mpsc::TryRecvError::Empty) = shutdown_rx.try_recv() {
            // Reported at most every DROP_WARNING_EVERY with a count, not once per burst.
            if events_dropped.load(Ordering::Relaxed) > 0
                && last_drop_warning.is_none_or(|at| at.elapsed() >= DROP_WARNING_EVERY)
            {
                let dropped = events_dropped.swap(0, Ordering::Relaxed);
                warn!(
                    dropped,
                    "Log files changed faster than they could be read; some log activity may be missing"
                );
                last_drop_warning = Some(Instant::now());
            }
            // A replacement is authoritative: rebuild the notify watcher so roots from
            // the previous desktop user cannot survive a fast-user-switch. Rebuilding
            // also re-arms paths whose old OS handle died after delete/recreate.
            while let Ok(directories) = dir_rx.try_recv() {
                let desired = replacement_watch_dirs(&directories);
                let mut replacement = match create_watcher(event_tx.clone(), events_dropped.clone())
                {
                    Ok(watcher) => Some(watcher),
                    Err(e) => {
                        warn!("Failed to rebuild file watcher: {e}");
                        None
                    }
                };
                let mut armed = HashSet::new();
                if let Some(next) = replacement.as_mut() {
                    for dir in &desired {
                        match arm_watch(next, dir) {
                            Ok(()) => {
                                armed.insert(dir.clone());
                            }
                            Err(e) => warn!("Cannot watch {}: {e}", dir.display()),
                        }
                    }
                }
                let removed = watched_dirs.difference(&armed).count();
                let added = armed.difference(&watched_dirs).count();
                let roots_changed = watched_set_changed(&watched_dirs, &armed);
                drop(std::mem::replace(&mut watcher, replacement));
                watched_dirs = armed;
                // Only drop accumulated events when the watched set ACTUALLY changed (a
                // fast-user-switch). main.rs re-sends the full set every 20 cycles even
                // when it is unchanged; clearing then wipes fresh, un-drained evidence
                // and races the decision loop's same-cycle drain.
                if roots_changed {
                    if let Ok(mut changes) = shared_clone.lock() {
                        changes.clear();
                    }
                    while event_rx.try_recv().is_ok() {}
                    memory = LogMemory::default();
                }
                info!(
                    dirs = watched_dirs.len(),
                    added, removed, "File watcher roots replaced"
                );
                if watcher.is_none() {
                    warn!("File watcher is inactive after rebuild failure");
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
                        // Checked before impersonating the user to read the file.
                        if !is_log_candidate(&path) {
                            continue;
                        }
                        let Some((size_bytes, log_event)) = parse_path(&path, true, &mut memory)
                        else {
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
                            if let Some(pending) =
                                guard.iter_mut().find(|pending| pending.path == change.path)
                            {
                                merge_change(pending, change);
                            } else {
                                if guard.len() >= RING_SIZE {
                                    guard.pop_front();
                                }
                                guard.push_back(change);
                            }
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

/// Linux log-directory watching is journald-first for v1 (see `signals::event_log`):
/// arbitrary path discovery/watching is deferred, so this always reports nothing to
/// watch. `monitoring.log_directories`' Windows-drive-shaped validation also means
/// `extra` is always empty in practice on Linux (see `config::normalize_log_directories`).
#[cfg(unix)]
pub fn discover_watch_dirs(_extra: &[String]) -> Option<Vec<PathBuf>> {
    Some(Vec::new())
}

/// Linux has no arbitrary-path file watcher yet (see [`discover_watch_dirs`]); this
/// keeps the exact same API shape as Windows — a live `SharedChanges`/`ShutdownHandle`/
/// `DirUpdateSender` triple — so `main.rs`'s call site needs zero `#[cfg]` of its own.
/// The background thread drains `DirUpdateSender` updates (so a caller can never block
/// sending one) and exits when `ShutdownHandle` is dropped, exactly like the Windows
/// watcher's shutdown contract.
#[cfg(unix)]
pub fn spawn(
    _directories: Vec<PathBuf>,
    _trigger: super::TriggerTx,
) -> (SharedChanges, ShutdownHandle, DirUpdateSender) {
    let shared: SharedChanges = Arc::new(Mutex::new(VecDeque::new()));
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let (dir_tx, dir_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();

    std::thread::spawn(move || {
        while let Err(std::sync::mpsc::TryRecvError::Empty) = shutdown_rx.try_recv() {
            let _ = dir_rx.recv_timeout(Duration::from_millis(500));
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A scratch log in its own temp folder, removed on drop even when a test fails.
    struct ScratchLog {
        dir: PathBuf,
        path: PathBuf,
    }

    impl ScratchLog {
        fn new(name: &str) -> std::io::Result<Self> {
            let dir =
                std::env::temp_dir().join(format!("eir-watch-test-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(name);
            Ok(Self { dir, path })
        }
    }

    impl Drop for ScratchLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_large_file_is_read_from_its_tail_without_the_partial_first_line() {
        // A file larger than the window: the newest line must survive and the far-older
        // head line must not — an earlier version skipped >64KB files entirely.
        let log = ScratchLog::new("large.log").unwrap();
        let path = log.path.clone();
        let filler = "x".repeat(MAX_READ_BYTES as usize);
        std::fs::write(
            &path,
            format!("HEAD-should-be-gone\n{filler}\nTAIL-ERROR-marker\n"),
        )
        .unwrap();
        let len = std::fs::metadata(&path).unwrap().len();

        let (tail, read_to) =
            read_chunk(&path, len - MAX_READ_BYTES, MAX_READ_BYTES, true).expect("tail");

        assert!(tail.len() as u64 <= MAX_READ_BYTES);
        assert!(tail.contains("TAIL-ERROR-marker"));
        assert!(!tail.contains("HEAD-should-be-gone"));
        assert_eq!(read_to, len);
    }

    #[test]
    fn a_line_still_being_written_is_left_for_the_next_read() {
        let log = ScratchLog::new("partial.log").unwrap();
        let path = log.path.clone();
        std::fs::write(&path, "line1\nERROR boom\nERROR half-writ").unwrap();
        let (text, read_to) = read_chunk(&path, 0, MAX_READ_BYTES, false).expect("chunk");
        assert!(text.contains("ERROR boom"));
        assert!(!text.contains("half-writ"));
        assert_eq!(read_to, "line1\nERROR boom\n".len() as u64);

        let (rest, _) = {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"ten\n").unwrap();
            read_chunk(&path, read_to, MAX_READ_BYTES, false).expect("rest")
        };
        assert_eq!(rest, "ERROR half-written\n");
    }

    #[test]
    fn a_log_reports_only_what_was_newly_written() {
        // The live bug: every write re-read the last 64KB and re-reported every old
        // error in it, so one harmless repeated line started an AI analysis every
        // couple of minutes.
        use std::io::Write;
        let log = ScratchLog::new("renderer.log").unwrap();
        let path = log.path.clone();
        std::fs::write(
            &path,
            "[16:10:01] [error] Permissions policy violation: encrypted-media\n",
        )
        .unwrap();
        let mut memory = LogMemory::default();
        let size = |path: &Path| std::fs::metadata(path).unwrap().len();

        let first = try_parse_log(&path, size(&path), &mut memory).expect("first sighting");
        assert_eq!(first.severity, "ERROR");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"[16:12:44] [error] Permissions policy violation: encrypted-media\n[16:12:45] [info] ready\n")
            .unwrap();
        assert!(
            try_parse_log(&path, size(&path), &mut memory).is_none(),
            "the same message again is not news"
        );
        assert!(
            try_parse_log(&path, size(&path), &mut memory).is_none(),
            "nothing appended means nothing to report"
        );

        file.write_all(b"[16:13:02] [error] Disk write failed: device not ready\n")
            .unwrap();
        let next = try_parse_log(&path, size(&path), &mut memory).expect("a new error");
        drop(file);
        assert_eq!(next.error_snippets.len(), 1);
        assert!(next.error_snippets[0].contains("Disk write failed"));
        assert!(!next.error_snippets[0].contains("Permissions policy"));
    }

    #[test]
    fn structured_state_files_are_not_read_as_logs() {
        let log = ScratchLog::new("session.json").unwrap();
        let path = log.path.clone();
        std::fs::write(&path, r#"{"status":"ok","errors":0,"crashed":false}"#).unwrap();
        let mut memory = LogMemory::default();
        let len = std::fs::metadata(&path).unwrap().len();
        let parsed = try_parse_log(&path, len, &mut memory);
        assert!(parsed.is_none());
    }

    #[test]
    fn eir_folders_and_the_user_temp_folder_are_ignored() {
        for ignored in [
            r"C:\Users\Swatto\AppData\Local\Temp\eir-opencode-4984-59",
            r"\\?\C:\Users\Swatto\AppData\Local\Temp\scoped_dir1_2\EBWebView",
            r"C:\Windows\SystemTemp\eir-codex-12-3",
            r"C:\ProgramData\Eir\staging",
            r"C:\Users\Swatto\AppData\Local\co.swatto.eir\EBWebView",
        ] {
            assert!(
                is_ignored_dir(Path::new(ignored)),
                "{ignored} must be ignored"
            );
        }
        for watched in [
            r"C:\Users\Swatto\AppData\Roaming\discord\logs",
            r"C:\Users\Swatto\AppData\Local\Battle.net\Logs",
            r"C:\Windows\Logs\CBS",
            r"C:\Windows\Temp",
            r"C:\ProgramData\NVIDIA Corporation\NVIDIA App\UXD",
            r"C:\Users\Swatto\AppData\Local\Weird Eir-like App",
        ] {
            assert!(
                !is_ignored_dir(Path::new(watched)),
                "{watched} must stay watched"
            );
        }
    }

    #[test]
    fn only_log_file_changes_are_queued() {
        use notify::event::{AccessKind, CreateKind, ModifyKind};
        let event =
            |kind: EventKind, path: &str| Ok(Event::new(kind).add_path(PathBuf::from(path)));
        let modify = EventKind::Modify(ModifyKind::Any);
        assert!(worth_queueing(&event(
            modify,
            r"C:\Users\Swatto\AppData\Roaming\discord\logs\renderer_js.log"
        )));
        assert!(worth_queueing(&event(
            EventKind::Create(CreateKind::File),
            r"C:\ProgramData\App\LOGS\Setup.LOG"
        )));
        for noise in [
            r"C:\Users\Swatto\AppData\Local\Google\Chrome\User Data\Default\Cache\f_00a1b2",
            r"C:\Users\Swatto\AppData\Roaming\Grok Bot\sentry\session.json",
            r"C:\Users\Swatto\AppData\Local\Temp\cq-gate.log",
            r"C:\Users\Swatto\AppData\Local\Temp\eir-codex-1-2\stdout.txt",
        ] {
            assert!(!worth_queueing(&event(modify, noise)), "{noise} was queued");
        }
        assert!(!worth_queueing(&event(
            EventKind::Access(AccessKind::Any),
            r"C:\Logs\app.log"
        )));
        assert!(
            worth_queueing(&Err(notify::Error::generic("watch failed"))),
            "errors still reach the thread so they are logged"
        );
    }

    #[test]
    fn changes_to_one_file_merge_into_a_single_pending_entry() {
        use crate::models::LogEvent;
        let change = |severity: &str, snippet: &str, at: i64| FileChange {
            path: PathBuf::from(r"C:\Logs\app.log"),
            kind: "modified".into(),
            size_bytes: 10,
            timestamp: chrono::DateTime::from_timestamp(at, 0).expect("ts"),
            log_event: Some(LogEvent {
                program: "App".into(),
                log_path: r"C:\Logs\app.log".into(),
                severity: severity.into(),
                error_snippets: vec![snippet.into()],
                content_excerpt: snippet.into(),
            }),
        };
        let mut pending = change("WARN", "WARN slow", 1);
        merge_change(&mut pending, change("ERROR", "ERROR broke", 2));
        merge_change(&mut pending, change("WARN", "WARN again", 3));
        let event = pending.log_event.expect("event");
        assert_eq!(event.severity, "ERROR", "the highest severity is kept");
        assert_eq!(
            event.error_snippets,
            ["WARN slow", "ERROR broke", "WARN again"]
        );
        assert_eq!(pending.timestamp.timestamp(), 3);

        let mut full = change("ERROR", "e0", 1);
        for index in 1..10 {
            merge_change(&mut full, change("ERROR", &format!("e{index}"), 1));
        }
        assert_eq!(
            full.log_event.expect("event").error_snippets.len(),
            super::super::log_parser::MAX_SNIPPETS
        );
    }

    #[test]
    fn discovery_stops_at_its_deadline() {
        let dir =
            std::env::temp_dir().join(format!("eir-discovery-deadline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recent.log"), "ERROR").unwrap();
        assert!(!has_recent_log_files(&dir, UNIX_EPOCH, 1, Instant::now()));
        assert!(has_recent_log_files(
            &dir,
            UNIX_EPOCH,
            1,
            Instant::now() + Duration::from_secs(5)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_real_root_change_drops_buffered_events() {
        // main.rs re-sends the discovered directory set every 20 cycles whether or not
        // it changed, and discovery order is not stable. A re-send of the same roots
        // (any order, duplicates included) must not count as a change — clearing then
        // wipes un-drained evidence. Only a fast-user-switch does.
        let a = PathBuf::from(r"C:\A");
        let b = PathBuf::from(r"C:\B");
        let sent = replacement_watch_dirs(&[a.clone(), b.clone()]);
        let resent = replacement_watch_dirs(&[b.clone(), a.clone(), a.clone()]);
        let switched = replacement_watch_dirs(&[PathBuf::from(r"C:\Other")]);

        assert!(!watched_set_changed(&sent, &resent));
        assert!(watched_set_changed(&sent, &switched));
        // A failed rebuild arms nothing — that IS a change, and still clears.
        assert!(watched_set_changed(&sent, &HashSet::new()));
    }

    #[test]
    fn configured_watch_dirs_reject_drive_roots_and_traversal() {
        assert!(configured_watch_dirs_current_user(&["C:\\".into()]).is_empty());
        assert!(configured_watch_dirs_current_user(&["C:\\Windows\\..".into()]).is_empty());
    }

    #[test]
    fn automatic_user_roots_come_from_the_active_profile() {
        let roots = profile_watch_roots(Path::new("D:\\Profiles\\Active"));
        assert_eq!(
            roots,
            [
                PathBuf::from("D:\\Profiles\\Active\\AppData\\Local"),
                PathBuf::from("D:\\Profiles\\Active\\AppData\\Roaming"),
            ]
        );
    }

    #[test]
    fn watch_guard_prevents_a_checked_path_from_being_replaced() {
        // Keep the path relative so this test exercises the guarded descendants,
        // without requiring directory-list access to every sandbox-owned ancestor.
        let base = PathBuf::from("target").join(format!("eir-watch-guard-{}", std::process::id()));
        let watched = base.join("watched");
        let moved = base.join("moved");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&watched).unwrap();

        let guard = WatchPathGuard::open(&watched).expect("lock watched path");
        assert!(
            std::fs::rename(&watched, &moved).is_err(),
            "a guarded path must not be replaceable before notify opens it"
        );
        drop(guard);
        std::fs::rename(&watched, &moved).expect("rename after guard release");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn replacement_update_drops_previous_user_roots_and_keeps_shared_roots() {
        let old_user = PathBuf::from(r"C:\Users\Old\AppData\Local\App");
        let new_user = PathBuf::from(r"C:\Users\New\AppData\Local\App");
        let shared = PathBuf::from(r"C:\ProgramData\App");
        let desired = replacement_watch_dirs(&[new_user.clone(), shared.clone()]);

        assert!(!desired.contains(&old_user));
        assert!(desired.contains(&new_user));
        assert!(desired.contains(&shared));
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn discovery_reports_nothing_to_watch_in_this_phase() {
        assert_eq!(discover_watch_dirs(&[]), Some(Vec::new()));
        assert_eq!(
            discover_watch_dirs(&["/var/log".to_string()]),
            Some(Vec::new())
        );
    }

    #[tokio::test]
    async fn spawn_accepts_directory_updates_without_blocking_and_shuts_down_cleanly() {
        let (trigger_tx, _trigger_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (shared, shutdown, updates) = spawn(Vec::new(), trigger_tx);
        assert!(drain(&shared).is_empty());
        // Never blocks, even though nothing is draining it on a tight loop.
        updates.send(vec![PathBuf::from("/tmp")]).expect("send");
        drop(shutdown); // signals the background thread to exit
    }
}
