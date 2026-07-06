//! Ask Eir attachment processing — runs in the tray (the user's session), so it can only
//! read what the user can already read, and it sends the service *content*, never a path
//! (the LocalSystem service must never open an attacker-named file). Images are decoded,
//! downscaled, and re-encoded JPEG so the pipe payload stays bounded; text files are
//! size-capped; folder picks are limited in file count + total bytes and skip
//! binaries/secrets.

use base64::Engine;
use eir_proto::AskAttachment;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Per text-file cap (chars). A long log is truncated, not rejected.
const MAX_TEXT_FILE_CHARS: usize = 100 * 1024;
/// Longest edge an image is downscaled to (Anthropic's recommended max) before re-encode.
const MAX_IMAGE_EDGE: u32 = 1568;
/// Max files pulled from a single folder pick.
const MAX_FOLDER_FILES: usize = 25;
/// Total text budget across a folder pick (bytes).
const MAX_FOLDER_TOTAL_BYTES: usize = 400 * 1024;
/// Hard cap on the pending-attachment list (across multiple picks).
pub const MAX_ATTACHMENTS: usize = 12;
/// Hard cap on the TOTAL content bytes of the pending list (across all picks), leaving
/// headroom under the 12 MiB pipe-line limit for the question + JSON overhead. Without
/// this, 12 near-1 MiB text files could exceed the pipe cap and silently drop the message.
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Max bytes read from an image file before decoding (a bound on the encoded size).
const MAX_IMAGE_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Max bytes read from a text file (then char-capped) — bounds memory even if the user
/// picks a multi-GB "text" file.
const MAX_TEXT_READ_BYTES: u64 = 1024 * 1024;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
/// Names skipped in a folder pick (secrets + noise).
const SKIP_NAMES: &[&str] = &["id_rsa", "id_ed25519", "id_dsa"];
/// Extensions skipped in a folder pick (secrets + binaries).
const SKIP_EXTS: &[&str] = &[
    "key", "pem", "pfx", "p12", "crt", "cer", "exe", "dll", "sys", "zip", "7z", "rar", "iso",
    "bin", "dat", "db", "sqlite",
];

/// Lightweight metadata returned to the frontend for display (not the content).
#[derive(Serialize, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub kind: String,
}

/// Process explicitly-picked files (each becomes an image if it decodes as one, else a
/// bounded text attachment; unreadable/binary non-images are skipped).
pub fn process_files(paths: &[PathBuf]) -> Vec<AskAttachment> {
    paths.iter().filter_map(|p| process_one(p)).collect()
}

/// Enumerate a folder's top-level files as bounded text attachments, skipping
/// binaries/secrets, capped by count + total bytes.
pub fn process_folder(dir: &Path) -> Vec<AskAttachment> {
    let mut out = Vec::new();
    let mut total = 0usize;
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        if out.len() >= MAX_FOLDER_FILES || total >= MAX_FOLDER_TOTAL_BYTES {
            break;
        }
        // Skip symlinks/junctions: `entry.metadata()` does NOT follow the link, so we can
        // detect one. A link named `notes.txt` could point at an SSH key elsewhere and
        // sail past the name/extension skip list — don't follow content out of the folder.
        let Ok(md) = entry.metadata() else {
            continue;
        };
        if md.file_type().is_symlink() || !md.is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip(&name, &path) {
            continue;
        }
        let Some(bytes) = read_capped(&path, MAX_TEXT_READ_BYTES) else {
            continue;
        };
        let Some(text) = as_text(&bytes) else {
            continue;
        };
        total += text.len();
        out.push(AskAttachment {
            name: format!("{base}/{name}"),
            kind: "text".to_string(),
            content: text,
            media_type: String::new(),
        });
    }
    out
}

fn process_one(path: &Path) -> Option<AskAttachment> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        if let Some(bytes) = read_capped(path, MAX_IMAGE_FILE_BYTES) {
            if let Some(img) = process_image(&name, &bytes) {
                return Some(img);
            }
        }
        // Fall through to text if it wasn't a decodable image after all.
    }
    let bytes = read_capped(path, MAX_TEXT_READ_BYTES)?;
    Some(AskAttachment {
        name,
        kind: "text".to_string(),
        content: as_text(&bytes)?,
        media_type: String::new(),
    })
}

/// Read at most `cap` bytes of a file (bounds memory before we know its size).
fn read_capped(path: &Path, cap: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(cap)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// Decode with explicit dimension/allocation limits so a decompression-bomb (or just a
/// huge) image can't OOM the tray.
fn decode_bounded(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}

/// A file's bytes as bounded UTF-8 text, or `None` if it looks binary (null byte or not
/// valid UTF-8).
fn as_text(bytes: &[u8]) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    Some(text.chars().take(MAX_TEXT_FILE_CHARS).collect())
}

/// Decode, downscale to `MAX_IMAGE_EDGE`, re-encode JPEG q80, base64. `None` if the bytes
/// don't decode as an image.
fn process_image(name: &str, bytes: &[u8]) -> Option<AskAttachment> {
    let img = decode_bounded(bytes)?;
    let img = if img.width().max(img.height()) > MAX_IMAGE_EDGE {
        img.resize(
            MAX_IMAGE_EDGE,
            MAX_IMAGE_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(AskAttachment {
        name: name.to_string(),
        kind: "image".to_string(),
        content: base64::engine::general_purpose::STANDARD.encode(&buf),
        media_type: "image/jpeg".to_string(),
    })
}

fn should_skip(name: &str, path: &Path) -> bool {
    let lower = name.to_lowercase();
    // Dotfiles (.env, .npmrc, …) and known secret filenames.
    if lower.starts_with('.') || SKIP_NAMES.iter().any(|s| lower == *s) {
        return true;
    }
    if let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        if SKIP_EXTS.contains(&ext.as_str()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_bytes_are_rejected_as_text() {
        assert!(as_text(b"hello\x00world").is_none());
        assert_eq!(as_text(b"plain text").as_deref(), Some("plain text"));
    }

    #[test]
    fn text_is_char_bounded() {
        let big = "a".repeat(MAX_TEXT_FILE_CHARS + 1000);
        let out = as_text(big.as_bytes()).unwrap();
        assert_eq!(out.chars().count(), MAX_TEXT_FILE_CHARS);
    }

    #[test]
    fn folder_skips_secrets_and_binaries() {
        use std::path::Path;
        assert!(should_skip(".env", Path::new(".env")));
        assert!(should_skip("id_rsa", Path::new("id_rsa")));
        assert!(should_skip("cert.pem", Path::new("cert.pem")));
        assert!(should_skip("app.exe", Path::new("app.exe")));
        assert!(!should_skip("app.log", Path::new("app.log")));
        assert!(!should_skip("config.ini", Path::new("config.ini")));
    }
}
