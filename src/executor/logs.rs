use anyhow::Result;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::info;

const CLEANABLE_EXTENSIONS: &[&str] = &["log", "tmp", "dmp", "etl", "blf", "regtrans-ms"];

pub fn cleanup(path: &str, days_old: u32) -> Result<String> {
    let dir = Path::new(path);
    if !dir.exists() {
        return Ok(format!("Directory '{path}' does not exist, skipping"));
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(days_old as u64 * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut deleted = 0u32;
    let mut skipped = 0u32;
    let mut bytes_freed: u64 = 0;

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = entry.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !CLEANABLE_EXTENSIONS.contains(&ext) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if modified >= cutoff {
            continue;
        }

        let size = meta.len();
        match std::fs::remove_file(p) {
            Ok(()) => {
                deleted += 1;
                bytes_freed += size;
                info!(path = %p.display(), "Deleted old log file");
            }
            Err(_) => skipped += 1,
        }
    }

    let mb_freed = bytes_freed as f64 / (1024.0 * 1024.0);
    Ok(format!(
        "Cleaned {deleted} files ({mb_freed:.1} MB freed), {skipped} locked/skipped \
         (>{days_old} days old in '{path}')"
    ))
}
