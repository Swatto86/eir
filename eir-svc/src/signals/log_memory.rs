//! What the log watcher remembers about each file: how far it has been read, and which
//! error and warning lines it has already reported.
//!
//! Before this memory every write to a watched log re-read its last 64 KiB and reported
//! every error line in it again. Busy apps rewrite the same harmless line all day
//! (Discord logs an identical "[error] Permissions policy violation" several times a
//! minute), so one such line started a fresh AI analysis every couple of minutes and
//! filled "What Eir noticed" with repeats. Pure bookkeeping — no I/O.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a reported line stays "already seen" for its file. It matches the analysis
/// heartbeat, so a line an app keeps writing is looked at again at most this often.
pub const SEEN_TTL: Duration = Duration::from_secs(6 * 3600);
/// Files remembered at once; the least recently written one is forgotten first.
const MAX_FILES: usize = 512;
/// Distinct reported lines remembered per file.
const MAX_LINES_PER_FILE: usize = 256;

#[derive(Default)]
pub struct LogMemory {
    files: HashMap<PathBuf, FileMemory>,
}

struct FileMemory {
    /// Byte offset up to which the file has been read.
    read_to: u64,
    touched: Instant,
    /// Line signature → when it was last reported.
    seen: HashMap<u64, Instant>,
}

/// Where the next read of a file starts.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadStart {
    pub offset: u64,
    /// The offset may fall inside a line, whose partial head must be skipped.
    pub mid_line: bool,
}

impl LogMemory {
    /// Where to read a file that is now `len` bytes long, reading at most `max` bytes.
    /// `None` when nothing was appended since the last read. A file seen for the first
    /// time, one that shrank (rotated or truncated), or one that grew by more than
    /// `max` is read from its last `max` bytes.
    pub fn read_start(
        &mut self,
        path: &Path,
        len: u64,
        max: u64,
        now: Instant,
    ) -> Option<ReadStart> {
        let tail = ReadStart {
            offset: len.saturating_sub(max),
            mid_line: len > max,
        };
        let Some(file) = self.files.get_mut(path) else {
            return (len > 0).then_some(tail);
        };
        file.touched = now;
        if len < file.read_to {
            // Rotated or truncated: start over from what the file holds now.
            file.read_to = 0;
            return (len > 0).then_some(tail);
        }
        if len == file.read_to {
            return None;
        }
        if len - file.read_to > max {
            return Some(tail);
        }
        Some(ReadStart {
            offset: file.read_to,
            mid_line: false,
        })
    }

    /// Record that the file has been read up to byte `to`.
    pub fn mark_read(&mut self, path: &Path, to: u64, now: Instant) {
        self.file(path, now).read_to = to;
    }

    /// Record a matched line for `path`: `true` the first time it is seen within
    /// [`SEEN_TTL`], `false` for a repeat, so a line is reported once, not on every write.
    pub fn first_sighting(&mut self, path: &Path, line: &str, now: Instant) -> bool {
        let signature = signature(line);
        let file = self.file(path, now);
        if file
            .seen
            .get(&signature)
            .is_some_and(|at| now.saturating_duration_since(*at) < SEEN_TTL)
        {
            return false;
        }
        if !file.seen.contains_key(&signature) && file.seen.len() >= MAX_LINES_PER_FILE {
            file.seen
                .retain(|_, at| now.saturating_duration_since(*at) < SEEN_TTL);
            if file.seen.len() >= MAX_LINES_PER_FILE {
                if let Some(oldest) = file.seen.iter().min_by_key(|(_, at)| **at).map(|(k, _)| *k) {
                    file.seen.remove(&oldest);
                }
            }
        }
        file.seen.insert(signature, now);
        true
    }

    fn file(&mut self, path: &Path, now: Instant) -> &mut FileMemory {
        if !self.files.contains_key(path) && self.files.len() >= MAX_FILES {
            if let Some(oldest) = self
                .files
                .iter()
                .min_by_key(|(_, file)| file.touched)
                .map(|(path, _)| path.clone())
            {
                self.files.remove(&oldest);
            }
        }
        let file = self
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| FileMemory {
                read_to: 0,
                touched: now,
                seen: HashMap::new(),
            });
        file.touched = now;
        file
    }
}

/// A line's identity without its volatile parts: case, punctuation, spacing, and every
/// token carrying a digit (timestamps, counters, ids, addresses), so the same message
/// written at another moment matches.
fn signature(line: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    for token in line
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        if token.chars().any(|c| c.is_ascii_digit()) {
            "#".hash(&mut hasher);
        } else {
            token.to_lowercase().hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u64 = 1000;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(format!(r"C:\Logs\{name}.log"))
    }

    #[test]
    fn a_new_file_is_read_from_its_tail() {
        let mut memory = LogMemory::default();
        let now = Instant::now();
        assert_eq!(
            memory.read_start(&path("small"), 300, MAX, now),
            Some(ReadStart {
                offset: 0,
                mid_line: false
            })
        );
        assert_eq!(
            memory.read_start(&path("big"), 5000, MAX, now),
            Some(ReadStart {
                offset: 4000,
                mid_line: true
            })
        );
        assert_eq!(memory.read_start(&path("empty"), 0, MAX, now), None);
    }

    #[test]
    fn a_growing_file_is_read_from_where_the_last_read_stopped() {
        let mut memory = LogMemory::default();
        let now = Instant::now();
        let log = path("app");
        memory.mark_read(&log, 300, now);
        assert_eq!(
            memory.read_start(&log, 450, MAX, now),
            Some(ReadStart {
                offset: 300,
                mid_line: false
            })
        );
        assert_eq!(
            memory.read_start(&log, 300, MAX, now),
            None,
            "nothing appended means nothing to read"
        );
    }

    #[test]
    fn a_rotated_or_hugely_grown_file_is_read_from_its_tail_again() {
        let mut memory = LogMemory::default();
        let now = Instant::now();
        let log = path("app");
        memory.mark_read(&log, 3000, now);
        assert_eq!(
            memory.read_start(&log, 200, MAX, now),
            Some(ReadStart {
                offset: 0,
                mid_line: false
            }),
            "a file that shrank was rotated or truncated"
        );
        memory.mark_read(&log, 200, now);
        assert_eq!(
            memory.read_start(&log, 9000, MAX, now),
            Some(ReadStart {
                offset: 8000,
                mid_line: true
            })
        );
        memory.mark_read(&log, 9000, now);
        assert_eq!(memory.read_start(&log, 0, MAX, now), None);
        assert_eq!(
            memory.read_start(&log, 100, MAX, now),
            Some(ReadStart {
                offset: 0,
                mid_line: false
            }),
            "growth after truncation to zero starts from the beginning"
        );
    }

    #[test]
    fn a_line_repeated_with_new_timestamps_is_reported_once_per_ttl() {
        let mut memory = LogMemory::default();
        let log = path("discord");
        let start = Instant::now();
        let first = "[2026-09-26 16:10:01.030] [error] Permissions policy violation: encrypted-media is not allowed.";
        let later = "[2026-09-26 16:12:44.901] [error] Permissions policy violation: encrypted-media is not allowed.";
        assert!(memory.first_sighting(&log, first, start));
        assert!(!memory.first_sighting(&log, later, start + Duration::from_secs(60)));
        assert!(
            memory.first_sighting(&log, "[error] Disk quota exceeded", start),
            "a different message is new"
        );
        assert!(
            memory.first_sighting(&path("other"), first, start),
            "the memory is per file"
        );
        assert!(
            memory.first_sighting(&log, later, start + SEEN_TTL),
            "after the TTL the line is reported again"
        );
    }

    #[test]
    fn memory_stays_bounded() {
        let mut memory = LogMemory::default();
        let now = Instant::now();
        for index in 0..(MAX_FILES + 10) {
            memory.mark_read(
                &path(&format!("f{index}")),
                1,
                now + Duration::from_millis(index as u64),
            );
        }
        assert_eq!(memory.files.len(), MAX_FILES);
        assert!(
            !memory.files.contains_key(&path("f0")),
            "the least recently touched file is forgotten first"
        );

        let log = path("busy");
        for index in 0..(MAX_LINES_PER_FILE + 10) {
            let line = format!("error kind{}", "x".repeat(index + 1));
            assert!(memory.first_sighting(&log, &line, now));
        }
        assert_eq!(memory.files[&log].seen.len(), MAX_LINES_PER_FILE);
    }
}
