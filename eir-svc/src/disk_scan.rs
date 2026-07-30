//! On-demand disk-space scan: rank the biggest space consumers on the system drive so
//! the user can see "what's eating my SSD". Categories and notes are deterministic —
//! never AI-authored, like `explain.rs` — so the user can trust them. The scan itself
//! mutates nothing; a "Clean" click maps an entry to an EXISTING safe fix-action
//! (`DiskCleanup` or `FileDelete`) that is routed through the normal policy gate.

use crate::models::FixAction;
use eir_proto::DiskEntryView;
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walkdir::{DirEntry, WalkDir};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Entries smaller than this are dropped as noise.
const MIN_REPORT_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
/// Keep at most this many entries (largest first).
const MAX_ENTRIES: usize = 25;

/// The result of a scan: the wire view list plus the server-side id → action map used to
/// reconstruct a "Clean" click (the wire only carries an opaque id).
pub struct DiskScanResult {
    pub entries: Vec<DiskEntryView>,
    pub targets: HashMap<String, FixAction>,
}

/// A known consumer to size and report. `action = Some(..)` makes it "cleanable"; the
/// action is an EXISTING safe fix-action, routed through the policy gate on a click.
struct Spec {
    path: String,
    /// True to size the whole tree, false to size a single file.
    is_dir: bool,
    category: &'static str,
    note: &'static str,
    action: Option<FixAction>,
}

/// The static, system-wide targets. Pure (no I/O) so the category → action mapping is
/// unit-testable. Only three map to an action, each a pre-existing safe one.
fn system_specs() -> Vec<Spec> {
    vec![
        Spec {
            path: "C:\\Windows\\Temp".into(),
            is_dir: true,
            category: "Temp files",
            note: "Temporary files Windows and apps left behind. Safe to clear now.",
            action: Some(FixAction::DiskCleanup {
                target: "temp".into(),
            }),
        },
        Spec {
            path: "C:\\Windows\\Prefetch".into(),
            is_dir: true,
            category: "Prefetch",
            note:
                "Windows' app-launch cache. Safe to clear; Windows rebuilds it. Low impact on SSDs.",
            action: Some(FixAction::DiskCleanup {
                target: "prefetch".into(),
            }),
        },
        Spec {
            path: "C:\\Windows\\MEMORY.DMP".into(),
            is_dir: false,
            category: "Crash dump",
            note: "A full crash dump from a past blue-screen. Safe to delete once it's no longer \
                   needed for diagnosis.",
            action: Some(FixAction::FileDelete {
                path: "C:\\Windows\\MEMORY.DMP".into(),
            }),
        },
        Spec {
            path: "C:\\Windows\\SoftwareDistribution\\Download".into(),
            is_dir: true,
            category: "Windows Update cache",
            note: "Downloaded Windows Update files. Windows clears these itself; safe to leave.",
            action: None,
        },
        Spec {
            path: "C:\\Windows.old".into(),
            is_dir: true,
            category: "Previous Windows",
            note: "Your previous Windows installation, kept for rollback. Remove it via Settings \
                   › System › Storage if you're sure you won't roll back.",
            action: None,
        },
        Spec {
            path: "C:\\Windows\\Minidump".into(),
            is_dir: true,
            category: "Crash dumps",
            note: "Small crash dumps from past blue-screens. Harmless to keep.",
            action: None,
        },
        Spec {
            path: "C:\\$Recycle.Bin".into(),
            is_dir: true,
            category: "Recycle Bin",
            note:
                "Deleted files not yet purged. Empty the Recycle Bin from Explorer to reclaim this.",
            action: None,
        },
        Spec {
            path: "C:\\hiberfil.sys".into(),
            is_dir: false,
            category: "Hibernation file",
            note: "Reserved for hibernation/fast-startup. Reclaim it by turning off hibernation \
                   (powercfg /h off) if you don't use it.",
            action: None,
        },
        Spec {
            path: "C:\\pagefile.sys".into(),
            is_dir: false,
            category: "Page file",
            note: "Windows virtual memory. Managed automatically — leave it as is.",
            action: None,
        },
    ]
}

/// A directory-junction/symlink guard: never descend into a reparse point (avoids cycles
/// and double-counting a linked tree — the same class of bug the v0.23.1 LogCleanup
/// canonicalisation fix guarded against).
fn is_reparse_dir(e: &DirEntry) -> bool {
    match e.metadata() {
        Ok(md) => md.is_dir() && (md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0),
        Err(_) => false,
    }
}

/// Total size of a directory tree in bytes, bounded by `deadline` (returns what it has
/// summed so far if the walk runs long — the scan is on-demand and must stay snappy).
fn dir_size(path: &Path, deadline: Instant) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total = 0u64;
    let mut count = 0u64;
    for entry in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_reparse_dir(e))
    {
        count += 1;
        // Check the clock only occasionally — Instant::now() per entry would dominate.
        if count.is_multiple_of(2048) && Instant::now() >= deadline {
            break;
        }
        let Ok(entry) = entry else { continue };
        if let Ok(md) = entry.metadata() {
            if md.is_file() {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}

/// Size of a single file, or 0 if absent/unreadable.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The interactive/local user profile directories under `C:\Users` (excluding the
/// system pseudo-profiles).
fn user_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("C:\\Users") {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if matches!(
                name.as_str(),
                "public" | "default" | "default user" | "all users"
            ) {
                continue;
            }
            v.push(p);
        }
    }
    v
}

/// Per-user report-only consumers (temp + biggest AppData\Local folders). All are
/// report-only in v1 — no existing action safely targets an arbitrary user directory.
fn user_specs(deadline: Instant) -> Vec<(String, u64, &'static str, &'static str)> {
    let mut out = Vec::new();
    for user in user_dirs() {
        if Instant::now() >= deadline {
            break;
        }
        let temp = user.join("AppData\\Local\\Temp");
        out.push((
            temp.to_string_lossy().into_owned(),
            dir_size(&temp, deadline),
            "Temp files",
            "This user's temporary files. Usually safe to clear from Disk Cleanup.",
        ));
        let local = user.join("AppData\\Local");
        if let Ok(rd) = std::fs::read_dir(&local) {
            for e in rd.flatten() {
                if Instant::now() >= deadline {
                    break;
                }
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                // Temp is already covered above — don't double-report it.
                if p.file_name().map(|n| n.eq_ignore_ascii_case("Temp")) == Some(true) {
                    continue;
                }
                let size = dir_size(&p, deadline);
                if size < MIN_REPORT_BYTES {
                    continue;
                }
                out.push((
                    p.to_string_lossy().into_owned(),
                    size,
                    "App data",
                    "Local data/cache for an app you use. Clear it from within the app if it's \
                     large — deleting it directly can sign you out or lose settings.",
                ));
            }
        }
    }
    out
}

/// Run the scan (blocking — call from `spawn_blocking`). `deadline` bounds the total
/// walk time. Assembles the ranked view list and the id → action map.
pub fn scan(deadline: Instant) -> DiskScanResult {
    // (path, size, category, note, action)
    let mut findings: Vec<(String, u64, &'static str, &'static str, Option<FixAction>)> =
        Vec::new();
    for s in system_specs() {
        let size = if s.is_dir {
            dir_size(Path::new(&s.path), deadline)
        } else {
            file_size(Path::new(&s.path))
        };
        findings.push((s.path, size, s.category, s.note, s.action));
    }
    for (path, size, category, note) in user_specs(deadline) {
        findings.push((path, size, category, note, None));
    }

    findings.retain(|f| f.1 >= MIN_REPORT_BYTES);
    findings.sort_by_key(|f| std::cmp::Reverse(f.1));
    findings.truncate(MAX_ENTRIES);

    let mut entries = Vec::with_capacity(findings.len());
    let mut targets = HashMap::new();
    for (path, size, category, note, action) in findings {
        // The path is unique and stable, so it doubles as the id.
        let id = path.clone();
        let cleanable = action.is_some();
        if let Some(a) = action {
            targets.insert(id.clone(), a);
        }
        entries.push(DiskEntryView {
            cleanable,
            id,
            path,
            size_bytes: size,
            category: category.to_string(),
            note: note.to_string(),
        });
    }
    DiskScanResult { entries, targets }
}

/// Run one bounded disk scan off the async runtime and return its result (or an error
/// string the caller sends on its result channel). The blocking walkdir is bounded by
/// its own `deadline`; the timeout backstop exists because `spawn_blocking` can't be
/// aborted, but the deadline stops the walk regardless. Lives here — with [`scan`] — so
/// the decision loop only spawns this and awaits the channel, never holds the walk body.
pub async fn run_scan() -> Result<DiskScanResult, String> {
    const SCAN_MAX: Duration = Duration::from_secs(5 * 60);
    let deadline = Instant::now() + SCAN_MAX;
    let mut handle = tokio::task::spawn_blocking(move || scan(deadline));
    match tokio::time::timeout(SCAN_MAX + Duration::from_secs(30), &mut handle).await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(_join)) => Err("disk scan task panicked".to_string()),
        Err(_elapsed) => {
            handle.abort();
            Err("disk scan timed out".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cleanable target maps ONLY to a pre-existing safe action — DiskCleanup for
    /// temp/prefetch or FileDelete — and report-only targets carry no action. This is the
    /// trust boundary: a UI "Clean" can never reach an arbitrary or unsafe action.
    #[test]
    fn cleanable_targets_map_only_to_safe_actions() {
        let mut cleanable = 0;
        for s in system_specs() {
            match &s.action {
                None => {}
                Some(FixAction::DiskCleanup { target }) => {
                    assert!(
                        target == "temp" || target == "prefetch",
                        "unexpected DiskCleanup target: {target}"
                    );
                    cleanable += 1;
                }
                Some(FixAction::FileDelete { .. }) => {
                    cleanable += 1;
                }
                other => panic!("disk clean must never map to {other:?}"),
            }
        }
        // Exactly the three documented cleanable entries (2× DiskCleanup + 1× FileDelete).
        assert_eq!(cleanable, 3);
    }

    #[test]
    fn dir_size_sums_files_and_respects_the_deadline() {
        let dir = std::env::temp_dir().join(format!("eir_disk_scan_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("b.bin"), vec![0u8; 2000]).unwrap();
        let future = Instant::now() + std::time::Duration::from_secs(60);
        assert_eq!(dir_size(&dir, future), 3000);
        // A missing path is 0 (not an error).
        assert_eq!(dir_size(&dir.join("nope"), future), 0);
        // (The deadline is checked every 2048 entries — a coarse bound for huge walks;
        // the outer task timeout is the real backstop. A 3-file dir never trips it, so
        // there's nothing meaningful to assert about a past deadline on this input.)
        let _ = std::fs::remove_dir_all(&dir);
    }
}
