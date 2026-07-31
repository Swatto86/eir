use crate::executor::logs::{checked_local_path, root_too_broad};
use crate::models::FileChange;
use crate::session::{active_user_session_id, system_drive_root, user_profile_dir_for_token};
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashSet, VecDeque};
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use windows::core::PCWSTR;
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

const RING_SIZE: usize = 50;
const EVENT_QUEUE_SIZE: usize = 256;
const MAX_READ_BYTES: u64 = 65_536;
const DISCOVERY_WINDOW_DAYS: u64 = 30;
const DISCOVERY_MAX: Duration = Duration::from_secs(30);
const MAX_AUTO_WATCH_DIRS: usize = 96;
const MAX_DISCOVERY_ENTRIES_PER_ROOT: usize = 4096;

pub const TEXT_EXTENSIONS: &[&str] = &[
    "log", "txt", "csv", "json", "xml", "ini", "cfg", "conf", "err", "out", "trace", "debug",
    "warn", "error", "info",
];

pub type SharedChanges = Arc<Mutex<VecDeque<FileChange>>>;
/// Send the complete desired directory set to the running watcher thread.
pub type DirUpdateSender = std::sync::mpsc::Sender<Vec<PathBuf>>;
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

impl Drop for ActiveUserImpersonation {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
            let _ = CloseHandle(self.0);
        }
    }
}

struct WatchPathGuard(Vec<HANDLE>);

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

impl Drop for WatchPathGuard {
    fn drop(&mut self) {
        for handle in self.0.drain(..).rev() {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

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

fn replacement_watch_dirs(directories: &[PathBuf]) -> HashSet<PathBuf> {
    directories.iter().cloned().collect()
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
                if sub.is_dir() && has_recent_log_files(&sub, cutoff, 2, deadline) {
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

fn profile_watch_roots(profile: &Path) -> [PathBuf; 3] {
    [
        profile.join("AppData\\Local"),
        profile.join("AppData\\Roaming"),
        profile.join("AppData\\Local\\Temp"),
    ]
}

/// Resolve configured roots while already impersonating the active desktop user.
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

fn create_watcher(
    event_tx: std::sync::mpsc::SyncSender<notify::Result<Event>>,
    overflowed: Arc<AtomicBool>,
) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(
        move |event| {
            if matches!(
                event_tx.try_send(event),
                Err(std::sync::mpsc::TrySendError::Full(_))
            ) {
                overflowed.store(true, Ordering::Relaxed);
            }
        },
        Config::default(),
    )
}

/// Start the file-watch background thread watching `directories`.
///
/// Returns a `ShutdownHandle` — dropping it signals the thread to exit — and a
/// `DirUpdateSender` for replacing the complete directory set at runtime.
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
    let event_overflowed = Arc::new(AtomicBool::new(false));

    let mut watcher = match create_watcher(event_tx.clone(), event_overflowed.clone()) {
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

        while let Err(std::sync::mpsc::TryRecvError::Empty) = shutdown_rx.try_recv() {
            if event_overflowed.swap(false, Ordering::Relaxed) {
                warn!("File watch event burst exceeded the bounded queue; some activity may be missing");
            }
            // A replacement is authoritative: rebuild the notify watcher so roots from
            // the previous desktop user cannot survive a fast-user-switch. Rebuilding
            // also re-arms paths whose old OS handle died after delete/recreate.
            while let Ok(directories) = dir_rx.try_recv() {
                let desired = replacement_watch_dirs(&directories);
                let mut replacement =
                    match create_watcher(event_tx.clone(), event_overflowed.clone()) {
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
                drop(std::mem::replace(&mut watcher, replacement));
                watched_dirs = armed;
                // Drop already-parsed and queued events from the old user's roots.
                if let Ok(mut changes) = shared_clone.lock() {
                    changes.clear();
                }
                while event_rx.try_recv().is_ok() {}
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
    fn configured_watch_dirs_reject_drive_roots_and_traversal() {
        assert!(configured_watch_dirs_current_user(&["C:\\".into()]).is_empty());
        assert!(configured_watch_dirs_current_user(&["C:\\Windows\\..".into()]).is_empty());
    }

    #[test]
    fn automatic_user_roots_come_from_the_active_profile() {
        let roots = profile_watch_roots(Path::new("D:\\Profiles\\Active"));
        assert_eq!(
            roots[0],
            PathBuf::from("D:\\Profiles\\Active\\AppData\\Local")
        );
        assert_eq!(
            roots[1],
            PathBuf::from("D:\\Profiles\\Active\\AppData\\Roaming")
        );
        assert_eq!(
            roots[2],
            PathBuf::from("D:\\Profiles\\Active\\AppData\\Local\\Temp")
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
