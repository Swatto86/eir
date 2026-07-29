//! Download, hash, and integrity-gate an AI-found native installer. Runs inside
//! the LocalSystem service, so there is no UAC — which raises the stakes: nothing
//! here trusts the AI. The URL host is re-validated on every redirect hop and the
//! final URL ([`super::plan::url_acceptable`]); the body must not be HTML and must
//! fit the size cap; a vendor-published SHA-256, if given, must match; and — the
//! key hardening over the old UI flow — the Authenticode signature is a HARD GATE
//! decided by [`signature_gate`], not merely recorded. Staging lives under a
//! SYSTEM/Administrators-only directory so a lesser-integrity process cannot swap
//! the file between download and launch.

use super::config::SignaturePolicy;
use super::plan::{url_acceptable, InstallPlan, InstallerKind};
use super::proc::{self, VERIFY};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::warn;

/// CREATE_NO_WINDOW — keep any spawned console (powershell/icacls) hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── SYSTEM-only staging ──────────────────────────────────────────────────────

/// Root of the installer staging area: `%ProgramData%\Eir\staging`, locked down to
/// SYSTEM + Administrators (fail-closed — staging is refused if the lockdown can't be
/// applied) so a lesser-integrity process can't tamper with a staged installer. The
/// re-hash before launch is then genuine belt-and-braces on top of the ACL.
fn staging_root() -> PathBuf {
    std::env::var("ProgramData")
        .ok()
        .map(|b| PathBuf::from(b).join("Eir").join("staging"))
        .unwrap_or_else(|| std::env::temp_dir().join("eir-staging"))
}

/// Strip inherited ACEs and grant only SYSTEM + Administrators full control, so the
/// staged installer cannot be read or replaced by a lesser-integrity process.
/// Returns whether it succeeded — this is a load-bearing control (see `ensure_root`),
/// not best-effort.
fn lock_down_acl(dir: &Path) -> bool {
    let p = dir.to_string_lossy().to_string();
    let status = std::process::Command::new("icacls")
        .args([
            p.as_str(),
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F", // BUILTIN\Administrators
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Ensure the staging root exists AND is locked down. FAIL CLOSED: if the ACL could
/// not be applied, return an error so no installer is ever staged (let alone run as
/// SYSTEM) from a directory a non-admin might be able to write.
fn ensure_root() -> std::io::Result<PathBuf> {
    let root = staging_root();
    std::fs::create_dir_all(&root)?;
    // Applied on every call, never latched behind a "already done" flag: `create_dir_all`
    // silently re-creates the root if anything removed it, and a re-created directory
    // inherits ProgramData's ACL (ordinary users may add files there) until `icacls` runs.
    // A latch would leave that window open for the rest of the service's life. One ~20ms
    // spawn per staged installer is nothing next to the download it is guarding, and a
    // transient failure only fails this one install, not every later one.
    if !lock_down_acl(&root) {
        warn!("could not lock down staging dir ACL: {}", root.display());
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "staging directory could not be locked to SYSTEM/Administrators",
        ));
    }
    Ok(root)
}

/// A unique per-download staging subdirectory under the locked-down root.
fn stage_dir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = ensure_root()?.join(format!("{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Delete any installer staging dirs left by a previous run (called at startup).
pub fn cleanup_stale_staging() {
    let _ = std::fs::remove_dir_all(staging_root());
}

// ── Authenticode signature ───────────────────────────────────────────────────

/// Parsed Authenticode result for a staged file.
#[derive(Debug, Clone, Default)]
pub struct Signature {
    /// `Get-AuthenticodeSignature` status: "Valid", "NotSigned", "HashMismatch", …
    pub status: String,
    /// Full signer certificate subject (empty if unsigned).
    pub subject: String,
    /// The CN extracted from the subject.
    pub cn: String,
}

impl Signature {
    pub fn is_valid(&self) -> bool {
        self.status.eq_ignore_ascii_case("Valid")
    }

    /// Human-readable summary for display/audit (never the gate decision).
    pub fn display(&self) -> String {
        if self.is_valid() {
            if self.cn.is_empty() {
                "signed".to_string()
            } else {
                format!("signed: {}", self.cn)
            }
        } else if self.status.eq_ignore_ascii_case("NotSigned") || self.status.is_empty() {
            "unsigned".to_string()
        } else {
            format!("untrusted ({})", self.status)
        }
    }
}

/// Read the Authenticode status + signer of a staged file via PowerShell.
async fn authenticode(file: &Path) -> Signature {
    let p = file.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{p}'; $subj=''; \
         if ($s.SignerCertificate) {{ $subj = $s.SignerCertificate.Subject }}; \
         Write-Output ($s.Status.ToString() + '|' + $subj)"
    );
    // A timed-out signature read yields non-"Valid" text, so the gate fails closed.
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", &script]);
    let (_code, out) = proc::run_capped_cmd(cmd, VERIFY).await;
    let raw = out.trim().to_string();

    let (status, subject) = raw
        .split_once('|')
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .unwrap_or((raw.clone(), String::new()));
    let cn = subject
        .split(',')
        .find_map(|p| p.trim().strip_prefix("CN="))
        .unwrap_or("")
        .to_string();
    Signature {
        status,
        subject,
        cn,
    }
}

/// HARD signature gate, decided in Rust before the installer is staged for launch.
/// Pure — separated from the PowerShell reader so it is exhaustively testable.
/// Error messages start with "signature rejected" so the failure classifies as a
/// terminal [`super::domain::ErrorCategory::SignatureRejected`].
pub fn signature_gate(
    sig: &Signature,
    policy: SignaturePolicy,
    expected_publisher: Option<&str>,
) -> Result<(), String> {
    match policy {
        SignaturePolicy::AllowUnsigned => Ok(()),
        SignaturePolicy::RequireValid => {
            if sig.is_valid() {
                Ok(())
            } else {
                Err(format!(
                    "signature rejected: installer is {}",
                    sig.display()
                ))
            }
        }
        SignaturePolicy::RequirePublisherMatch => {
            if !sig.is_valid() {
                return Err(format!(
                    "signature rejected: installer is {}",
                    sig.display()
                ));
            }
            // Exact (case-insensitive) signer-CN equality, not a substring test, so a
            // short common substring can't satisfy the pin. NOTE: `expected_publisher`
            // is currently AI-sourced, so this policy is a tripwire ("valid signature
            // whose CN equals the claimed publisher"), not a true vendor pin — a
            // trusted per-app publisher map would harden it further.
            match expected_publisher.map(str::trim).filter(|p| !p.is_empty()) {
                Some(pubr) if sig.cn.eq_ignore_ascii_case(pubr) => Ok(()),
                Some(pubr) => Err(format!(
                    "signature rejected: signer '{}' does not match expected publisher '{pubr}'",
                    if sig.cn.is_empty() { &sig.subject } else { &sig.cn }
                )),
                None => Err(
                    "signature rejected: publisher match required but no expected publisher is known"
                        .to_string(),
                ),
            }
        }
    }
}

// ── Streaming download with integrity checks ─────────────────────────────────

/// Reject a response whose headers already disqualify it: an HTML body (a download
/// page, not an installer) or a declared length over the cap. Pure.
fn response_check(
    content_type: Option<&str>,
    content_length: Option<u64>,
    max_bytes: u64,
) -> Result<(), String> {
    if let Some(ct) = content_type {
        let ct = ct.to_lowercase();
        if ct.contains("text/html") || ct.contains("application/xhtml") {
            return Err(format!("download is a web page, not an installer ({ct})"));
        }
    }
    if let Some(len) = content_length {
        if len > max_bytes {
            return Err(format!("installer is too large ({len} bytes)"));
        }
    }
    Ok(())
}

/// Compare a vendor-published hash (if any) against what we actually downloaded.
/// Pure. The message says "SHA-256 mismatch" so it classifies as a terminal
/// [`super::domain::ErrorCategory::HashMismatch`].
pub fn verify_downloaded_hash(expected: Option<&str>, got: &str) -> Result<(), String> {
    match expected {
        Some(exp) if !exp.eq_ignore_ascii_case(got) => Err(format!(
            "download SHA-256 mismatch (expected {exp}, got {got}) — aborted"
        )),
        _ => Ok(()),
    }
}

/// Stream a verified response body to `dest`, hashing as it writes and enforcing the
/// size cap on the actual bytes (not just the declared length). Returns the lowercase
/// hex SHA-256. Split from [`stream_download`] so the streaming/hash/cap mechanics are
/// testable against a localhost server without the production host gate in the way.
async fn stream_body(
    resp: reqwest::Response,
    dest: &Path,
    max_bytes: u64,
) -> Result<String, String> {
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    response_check(content_type.as_deref(), resp.content_length(), max_bytes)?;

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("could not create staged file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(format!("installer exceeded the {max_bytes}-byte cap"));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write failed: {e}"))?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Stream a download to `dest`, enforcing https + an acceptable host on every
/// redirect hop and the final URL, a non-HTML body, and the size cap. Hashes as it
/// writes and returns the lowercase hex SHA-256.
async fn stream_download(
    url: &str,
    name: &str,
    dest: &Path,
    max_bytes: u64,
) -> Result<String, String> {
    let name_owned = name.to_string();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        // Re-apply the FULL initial-URL gate (https, no creds/port, no raw IP, no
        // IDN, acceptable host) to every hop — not just scheme + host.
        match url_acceptable(attempt.url(), &name_owned) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(format!("blocked redirect ({reason})")),
        }
    });
    let client = reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download request failed: {e}"))?;
    // Re-check the FINAL URL after redirects with the same strict gate.
    if let Err(reason) = url_acceptable(resp.url(), name) {
        return Err(format!("download landed on an unacceptable URL ({reason})"));
    }
    stream_body(resp, dest, max_bytes).await
}

// ── Stage + check (download -> hash gate -> signature gate) ───────────────────

/// A downloaded, integrity-checked installer ready to run.
pub struct Staged {
    pub dir: PathBuf,
    pub file: PathBuf,
    pub kind: InstallerKind,
    /// SHA-256 of the executable installer; re-checked before launch (TOCTOU).
    pub sha256: String,
    /// Authenticode result (for audit/display; the gate already passed).
    pub signature: Signature,
}

const MAX_ARCHIVE_ENTRIES: usize = 10_000;

#[derive(Clone)]
struct ArchiveCandidate {
    index: usize,
    name: String,
    kind: InstallerKind,
    size: u64,
}

fn candidate(index: usize, name: String, size: u64) -> Option<ArchiveCandidate> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized.contains(':')
        || normalized
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return None;
    }
    let lower = normalized.to_lowercase();
    let kind = if lower.ends_with(".msi") {
        InstallerKind::Msi
    } else if lower.ends_with(".exe") {
        InstallerKind::Exe
    } else {
        return None;
    };
    Some(ArchiveCandidate {
        index,
        name: normalized,
        kind,
        size,
    })
}

fn select_candidate(
    candidates: Vec<ArchiveCandidate>,
    requested_path: Option<&str>,
    archive: &str,
    max_bytes: u64,
) -> Result<ArchiveCandidate, String> {
    let selected = match requested_path {
        Some(requested) => {
            let mut matches = candidates
                .into_iter()
                .filter(|entry| entry.name.eq_ignore_ascii_case(requested));
            let selected = matches.next().ok_or_else(|| {
                format!("installer not found in {archive}: no entry named '{requested}'")
            })?;
            if matches.next().is_some() {
                return Err(format!(
                    "{archive} contains multiple matches for '{requested}'"
                ));
            }
            selected
        }
        None => {
            let mut candidates = candidates.into_iter();
            let selected = candidates.next().ok_or_else(|| {
                format!("installer not found in {archive}: no .exe or .msi entry")
            })?;
            if candidates.next().is_some() {
                return Err(format!(
                    "{archive} contains multiple .exe/.msi files; no exact installer path was provided"
                ));
            }
            selected
        }
    };
    if selected.size > max_bytes {
        return Err(format!(
            "installer inside {archive} is too large ({} bytes)",
            selected.size
        ));
    }
    Ok(selected)
}

fn write_installer(
    reader: &mut dyn Read,
    dir: &Path,
    kind: InstallerKind,
    max_bytes: u64,
) -> std::io::Result<(PathBuf, InstallerKind, String)> {
    let dest = dir.join(match kind {
        InstallerKind::Msi => "installer.msi",
        InstallerKind::Exe => "installer.exe",
        _ => return Err(std::io::Error::other("nested archives are not supported")),
    });
    let mut output = std::fs::File::create(&dest)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("installer exceeded the {max_bytes}-byte cap"),
            ));
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    output.flush()?;
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((dest, kind, sha256))
}

/// Extract one Windows installer from a ZIP into a fixed staging filename. Nothing
/// else in the archive is written, so traversal entries cannot escape staging.
fn extract_zip_installer(
    archive_path: &Path,
    dir: &Path,
    requested_path: Option<&str>,
    max_bytes: u64,
) -> Result<(PathBuf, InstallerKind, String), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("could not open downloaded ZIP: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("download is not a valid ZIP: {e}"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!("ZIP contains too many entries ({})", archive.len()));
    }

    let mut candidates = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("could not inspect ZIP entry: {e}"))?;
        if entry.is_dir() || entry.enclosed_name().is_none() {
            continue;
        }
        if let Some(candidate) = candidate(index, entry.name().to_string(), entry.size()) {
            candidates.push(candidate);
        }
    }
    let selected = select_candidate(candidates, requested_path, "ZIP", max_bytes)?;
    let mut entry = archive
        .by_index(selected.index)
        .map_err(|e| format!("could not read ZIP installer: {e}"))?;
    write_installer(&mut entry, dir, selected.kind, max_bytes)
        .map_err(|e| format!("could not extract ZIP installer: {e}"))
}

fn tar_reader(path: &Path, gzip: bool) -> Result<Box<dyn Read>, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("could not open downloaded archive: {e}"))?;
    if gzip {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(file)))
    } else {
        Ok(Box::new(file))
    }
}

fn extract_tar_installer(
    archive_path: &Path,
    dir: &Path,
    requested_path: Option<&str>,
    max_bytes: u64,
    gzip: bool,
) -> Result<(PathBuf, InstallerKind, String), String> {
    let label = if gzip { "TAR.GZ" } else { "TAR" };
    let mut archive = tar::Archive::new(tar_reader(archive_path, gzip)?);
    let entries = archive
        .entries()
        .map_err(|e| format!("download is not a valid {label}: {e}"))?;
    let mut candidates = Vec::new();
    let mut expanded_size = 0u64;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_ARCHIVE_ENTRIES {
            return Err(format!("{label} contains too many entries"));
        }
        let entry = entry.map_err(|e| format!("could not inspect {label} entry: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let size = entry.size();
        expanded_size = expanded_size.saturating_add(size);
        if expanded_size > max_bytes {
            return Err(format!(
                "{label} expanded contents exceed the {max_bytes}-byte cap"
            ));
        }
        let Some(name) = entry
            .path()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
        else {
            continue;
        };
        if let Some(candidate) = candidate(index, name, size) {
            candidates.push(candidate);
        }
    }
    let selected = select_candidate(candidates, requested_path, label, max_bytes)?;

    let mut archive = tar::Archive::new(tar_reader(archive_path, gzip)?);
    for (index, entry) in archive
        .entries()
        .map_err(|e| format!("download is not a valid {label}: {e}"))?
        .enumerate()
    {
        let mut entry = entry.map_err(|e| format!("could not read {label} installer: {e}"))?;
        if index == selected.index {
            return write_installer(&mut entry, dir, selected.kind, max_bytes)
                .map_err(|e| format!("could not extract {label} installer: {e}"));
        }
    }
    Err(format!("installer disappeared while reading {label}"))
}

fn extract_7z_installer(
    archive_path: &Path,
    dir: &Path,
    requested_path: Option<&str>,
    max_bytes: u64,
) -> Result<(PathBuf, InstallerKind, String), String> {
    let archive = sevenz_rust2::Archive::open(archive_path)
        .map_err(|e| format!("download is not a valid 7z archive: {e}"))?;
    if archive.files.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!(
            "7z contains too many entries ({})",
            archive.files.len()
        ));
    }
    let mut candidates = Vec::new();
    let mut expanded_size = 0u64;
    for (index, entry) in archive.files.iter().enumerate() {
        if entry.is_directory {
            continue;
        }
        expanded_size = expanded_size.saturating_add(entry.size);
        if expanded_size > max_bytes {
            return Err(format!(
                "7z expanded contents exceed the {max_bytes}-byte cap"
            ));
        }
        if let Some(candidate) = candidate(index, entry.name.clone(), entry.size) {
            candidates.push(candidate);
        }
    }
    let selected = select_candidate(candidates, requested_path, "7z", max_bytes)?;
    let mut extracted = None;
    sevenz_rust2::decompress_file_with_extract_fn(
        archive_path,
        dir,
        |entry, reader, _unused_path| {
            if entry.name.eq_ignore_ascii_case(&selected.name) {
                extracted = Some(write_installer(reader, dir, selected.kind, max_bytes)?);
                return Ok(false);
            }
            Ok(true)
        },
    )
    .map_err(|e| format!("could not extract 7z installer: {e}"))?;
    extracted.ok_or_else(|| "installer disappeared while reading 7z".to_string())
}

/// Download the plan's installer, hard-fail on a provided-hash mismatch, then apply
/// the signature policy as a HARD gate. Any rejection deletes the staging dir and
/// returns a reason that classifies as a terminal integrity failure.
pub async fn download_and_check(
    plan: &InstallPlan,
    max_bytes: u64,
    policy: SignaturePolicy,
) -> Result<Staged, String> {
    let dir = stage_dir().map_err(|e| format!("could not create staging dir: {e}"))?;
    let artifact = dir.join(match plan.kind {
        InstallerKind::Msi => "installer.msi",
        InstallerKind::Exe => "installer.exe",
        InstallerKind::Zip => "installer.zip",
        InstallerKind::SevenZ => "installer.7z",
        InstallerKind::Tar => "installer.tar",
        InstallerKind::TarGz => "installer.tar.gz",
    });

    let result = async {
        let artifact_sha =
            stream_download(&plan.installer_url, &plan.name, &artifact, max_bytes).await?;
        verify_downloaded_hash(plan.sha256.as_deref(), &artifact_sha)?;
        let (file, kind, sha) = if plan.kind.is_archive() {
            let archive = artifact.clone();
            let extract_dir = dir.clone();
            let requested = plan.archive_installer_path.clone();
            let archive_kind = plan.kind;
            tokio::task::spawn_blocking(move || match archive_kind {
                InstallerKind::Zip => {
                    extract_zip_installer(&archive, &extract_dir, requested.as_deref(), max_bytes)
                }
                InstallerKind::SevenZ => {
                    extract_7z_installer(&archive, &extract_dir, requested.as_deref(), max_bytes)
                }
                InstallerKind::Tar => extract_tar_installer(
                    &archive,
                    &extract_dir,
                    requested.as_deref(),
                    max_bytes,
                    false,
                ),
                InstallerKind::TarGz => extract_tar_installer(
                    &archive,
                    &extract_dir,
                    requested.as_deref(),
                    max_bytes,
                    true,
                ),
                InstallerKind::Exe | InstallerKind::Msi => unreachable!(),
            })
            .await
            .map_err(|e| format!("archive extraction task failed: {e}"))??
        } else {
            (artifact.clone(), plan.kind, artifact_sha)
        };
        let signature = authenticode(&file).await;
        signature_gate(&signature, policy, plan.expected_publisher.as_deref())?;
        Ok::<_, String>(Staged {
            dir: dir.clone(),
            file: file.clone(),
            kind,
            sha256: sha,
            signature,
        })
    }
    .await;

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_check_rejects_html_and_oversize() {
        assert!(response_check(Some("application/octet-stream"), Some(1000), 2000).is_ok());
        assert!(response_check(Some("text/html; charset=utf-8"), Some(10), 2000).is_err());
        assert!(response_check(Some("application/xhtml+xml"), None, 2000).is_err());
        assert!(response_check(Some("application/octet-stream"), Some(3000), 2000).is_err());
        // No headers at all -> can't reject here (the streaming cap still applies).
        assert!(response_check(None, None, 2000).is_ok());
    }

    #[test]
    fn verify_downloaded_hash_is_case_insensitive_and_strict() {
        assert!(verify_downloaded_hash(None, "abcd").is_ok());
        assert!(verify_downloaded_hash(Some("ABCD"), "abcd").is_ok());
        assert!(verify_downloaded_hash(Some("abcd"), "abce").is_err());
    }

    fn sig(status: &str, subject: &str) -> Signature {
        let cn = subject
            .split(',')
            .find_map(|p| p.trim().strip_prefix("CN="))
            .unwrap_or("")
            .to_string();
        Signature {
            status: status.to_string(),
            subject: subject.to_string(),
            cn,
        }
    }

    #[test]
    fn signature_gate_require_valid_blocks_unsigned() {
        let valid = sig("Valid", "CN=Mozilla Corporation, O=Mozilla");
        let unsigned = sig("NotSigned", "");
        assert!(signature_gate(&valid, SignaturePolicy::RequireValid, None).is_ok());
        let err = signature_gate(&unsigned, SignaturePolicy::RequireValid, None).unwrap_err();
        assert!(err.starts_with("signature rejected"), "{err}");
    }

    #[test]
    fn signature_gate_allow_unsigned_passes_anything() {
        let unsigned = sig("NotSigned", "");
        assert!(signature_gate(&unsigned, SignaturePolicy::AllowUnsigned, None).is_ok());
    }

    #[test]
    fn signature_gate_publisher_match_requires_subject_match() {
        let valid = sig("Valid", "CN=Mozilla Corporation, O=Mozilla, C=US");
        // Matching publisher -> ok.
        assert!(signature_gate(
            &valid,
            SignaturePolicy::RequirePublisherMatch,
            Some("Mozilla Corporation")
        )
        .is_ok());
        // Wrong publisher -> rejected.
        assert!(signature_gate(
            &valid,
            SignaturePolicy::RequirePublisherMatch,
            Some("Evil Corp")
        )
        .is_err());
        // No expected publisher to match against -> rejected (can't satisfy policy).
        assert!(signature_gate(&valid, SignaturePolicy::RequirePublisherMatch, None).is_err());
        // Invalid signature -> rejected regardless of publisher.
        let untrusted = sig("HashMismatch", "CN=Mozilla Corporation");
        assert!(signature_gate(
            &untrusted,
            SignaturePolicy::RequirePublisherMatch,
            Some("Mozilla Corporation")
        )
        .is_err());
    }

    #[test]
    fn zip_extracts_only_the_named_bounded_installer() {
        use zip::write::SimpleFileOptions;

        let dir = std::env::temp_dir().join(format!("eir-zip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("release.zip");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(archive_file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("../evil.exe", options).unwrap();
        writer.write_all(b"evil").unwrap();
        writer.start_file("portable/tool.exe", options).unwrap();
        writer.write_all(b"portable").unwrap();
        writer.start_file("setup/tool.msi", options).unwrap();
        writer.write_all(b"installer").unwrap();
        writer.finish().unwrap();

        assert!(extract_zip_installer(&archive_path, &dir, None, 100).is_err());
        let (file, kind, sha) =
            extract_zip_installer(&archive_path, &dir, Some("setup/tool.msi"), 100).unwrap();
        assert_eq!(kind, InstallerKind::Msi);
        assert_eq!(std::fs::read(file).unwrap(), b"installer");
        assert_eq!(
            sha,
            "9c0d294c05fc1d88d698034609bb81c0c69196327594e4c69d2915c80fd9850c"
        );
        assert!(extract_zip_installer(&archive_path, &dir, Some("setup/tool.msi"), 4).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_test_tar<W: Write>(writer: W) -> W {
        let mut archive = tar::Builder::new(writer);
        let contents = b"installer";
        let mut header = tar::Header::new_gnu();
        header.set_path("setup/tool.exe").unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append(&header, contents.as_slice()).unwrap();
        archive.finish().unwrap();
        archive.into_inner().unwrap()
    }

    #[test]
    fn extracts_installers_from_7z_tar_and_tar_gz() {
        let dir = std::env::temp_dir().join(format!("eir-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let tar_path = dir.join("release.tar");
        let _ = write_test_tar(std::fs::File::create(&tar_path).unwrap());
        let (file, kind, _) = extract_tar_installer(&tar_path, &dir, None, 100, false).unwrap();
        assert_eq!(kind, InstallerKind::Exe);
        assert_eq!(std::fs::read(file).unwrap(), b"installer");

        let targz_path = dir.join("release.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&targz_path).unwrap(),
            flate2::Compression::fast(),
        );
        write_test_tar(encoder).finish().unwrap();
        let (file, kind, _) =
            extract_tar_installer(&targz_path, &dir, Some("setup/tool.exe"), 100, true).unwrap();
        assert_eq!(kind, InstallerKind::Exe);
        assert_eq!(std::fs::read(file).unwrap(), b"installer");

        let source = dir.join("seven-source");
        std::fs::create_dir_all(source.join("setup")).unwrap();
        std::fs::write(source.join("setup").join("tool.exe"), b"installer").unwrap();
        let sevenz_path = dir.join("release.7z");
        sevenz_rust2::compress_to_path(&source, &sevenz_path).unwrap();
        let (file, kind, _) =
            extract_7z_installer(&sevenz_path, &dir, Some("setup/tool.exe"), 100).unwrap();
        assert_eq!(kind, InstallerKind::Exe);
        assert_eq!(std::fs::read(file).unwrap(), b"installer");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A localhost HTTP/1.1 server that returns one fixed response, so the stream/
    /// hash/cap mechanics can be exercised end-to-end without the production host
    /// gate (which deliberately rejects localhost).
    async fn serve_once(content_type: &str, body: Vec<u8>) -> String {
        use tokio::io::AsyncReadExt; // AsyncWriteExt is already in scope from the module
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ct = content_type.to_string();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await; // consume the request line/headers
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://{addr}/installer.bin")
    }

    #[tokio::test]
    async fn stream_body_hashes_and_caps() {
        // Known body -> known SHA-256 (computed independently below).
        let body = b"eir installer payload".to_vec();
        let expected: String = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let url = serve_once("application/octet-stream", body.clone()).await;
        let dest = std::env::temp_dir().join(format!("eir-dl-test-{}.bin", std::process::id()));
        let got = reqwest::get(&url).await.unwrap();
        let sha = stream_body(got, &dest, 1024 * 1024)
            .await
            .expect("download ok");
        assert_eq!(sha, expected);
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn stream_body_rejects_oversize_via_streaming_cap() {
        // Serve more bytes than the cap; the streaming counter must abort it.
        let body = vec![0u8; 4096];
        let url = serve_once("application/octet-stream", body).await;
        let dest = std::env::temp_dir().join(format!("eir-dl-cap-{}.bin", std::process::id()));
        let got = reqwest::get(&url).await.unwrap();
        let err = stream_body(got, &dest, 1024).await.unwrap_err();
        assert!(err.contains("cap") || err.contains("too large"), "{err}");
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn stream_body_rejects_html() {
        let url = serve_once("text/html; charset=utf-8", b"<html>nope</html>".to_vec()).await;
        let dest = std::env::temp_dir().join(format!("eir-dl-html-{}.bin", std::process::id()));
        let got = reqwest::get(&url).await.unwrap();
        let err = stream_body(got, &dest, 1024 * 1024).await.unwrap_err();
        assert!(err.contains("web page"), "{err}");
        let _ = std::fs::remove_file(&dest);
    }
}
