//! Post-update verification: confirm an app now reports the version we expected to
//! install. Reads the installed version from winget (by id or by name) or, as a
//! second signal for native installs, an installed exe's ProductVersion, and maps
//! it to a [`Verification`] verdict via [`super::version::classify_version`]. The
//! comparison itself is pure and unit-tested in `version`; this module is the I/O.

use super::domain::Verification;
use super::methods::winget;
use super::proc::{self, VERIFY};
use super::version::classify_version;
use super::winget_parse::{column, winget_table};
use std::path::Path;
use std::time::Duration;

/// What to read the installed version from.
pub enum VerifyTarget {
    Winget {
        id: String,
    },
    ByName {
        name: String,
        verify_exe: Option<String>,
    },
}

/// A binary's ProductVersion is often a 4-part FILEVERSION that trails the
/// marketing/release version, so don't hard-fail a native install on an exe-fallback
/// "mismatch" — soften it to Unverified. Pure.
fn soften_exe_fallback(verdict: Verification, from_exe: bool) -> Verification {
    if from_exe && verdict == Verification::Mismatch {
        Verification::Unverified
    } else {
        verdict
    }
}

/// A verdict worth acting on immediately. A first-read `Mismatch` is re-read once:
/// right after a successful install the old version can still be the registered one
/// for a moment, and reporting that as a failed update is worse than a 2s wait. A
/// genuinely failed install still reads `Mismatch` the second time. Pure.
fn settled(verdict: Verification) -> bool {
    verdict != Verification::Mismatch
}

/// Confirm an app now reports `expected` (or newer). Returns (verdict, found
/// version). One short retry absorbs the ARP-registration lag right after a fresh
/// install — whether that shows up as no readable version at all or as the previous
/// version still being registered.
pub async fn verify_app(target: &VerifyTarget, expected: &str) -> (Verification, String) {
    let mut last = (Verification::Unverified, String::new());
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let (found, from_exe) = match target {
            VerifyTarget::Winget { id } => (winget_installed_version(id).await, false),
            VerifyTarget::ByName { name, verify_exe } => {
                let v = winget_installed_version_by_name(name).await;
                match (v, verify_exe) {
                    (Some(v), _) => (Some(v), false),
                    (None, Some(exe)) => (exe_file_version(exe).await, true),
                    (None, None) => (None, false),
                }
            }
        };
        if let Some(found) = found {
            let verdict = soften_exe_fallback(classify_version(&found, expected), from_exe);
            if settled(verdict) {
                return (verdict, found);
            }
            last = (verdict, found);
        }
    }
    last
}

/// Read an installed app's version from `winget list --id <id> --exact`.
async fn winget_installed_version(id: &str) -> Option<String> {
    let (_code, text) = winget::run_winget(
        vec![
            "list".to_string(),
            "--id".to_string(),
            id.to_string(),
            "--exact".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        VERIFY,
    )
    .await;
    let (offsets, rows) = winget_table(&text);
    rows.first()
        .map(|r| column(&offsets, r, "Version"))
        .filter(|v| !v.is_empty())
}

/// Read an installed app's version from `winget list --name <name>`, matching the
/// row whose Name overlaps the queried name (display names are fuzzy).
async fn winget_installed_version_by_name(name: &str) -> Option<String> {
    let (_code, text) = winget::run_winget(
        vec![
            "list".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        VERIFY,
    )
    .await;
    let (offsets, rows) = winget_table(&text);
    let lname = name.to_lowercase();
    // An exact (case-insensitive) name match is authoritative. Otherwise collect the
    // containment matches: a bare bidirectional `contains` verified "Git" against
    // "GitHub Desktop", so only accept a containment result when it is UNAMBIGUOUS
    // (a single distinct version) — else return None (→ Unverified) rather than
    // reading the wrong app's version and declaring a false Verified/Mismatch.
    let mut contains: Vec<String> = Vec::new();
    for r in &rows {
        let n = column(&offsets, r, "Name").to_lowercase();
        if n.is_empty() {
            continue;
        }
        let v = column(&offsets, r, "Version");
        if v.is_empty() {
            continue;
        }
        if n == lname {
            return Some(v);
        }
        if n.contains(&lname) || lname.contains(&n) {
            contains.push(v);
        }
    }
    match contains.first() {
        Some(first) if contains.iter().all(|v| v == first) => Some(first.clone()),
        _ => None,
    }
}

/// Read FileVersion/ProductVersion of an absolute exe path (a second confirmation
/// signal for native installs). Returns None for relative paths or missing files.
/// Whether `path` is an absolute drive-letter path (`C:\...`). Rejects relative paths
/// AND UNC / verbatim-UNC paths (`\\host\share\...`): `is_absolute()` is true for UNC,
/// which Windows would resolve over SMB, making the LocalSystem machine account
/// authenticate to an attacker-controlled host (a forced-auth / NTLM-relay primitive).
/// The AI supplies this path, so the guard must be explicit.
fn is_local_drive_path(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        && matches!(
            p.components().next(),
            Some(std::path::Component::Prefix(pre)) if matches!(pre.kind(), std::path::Prefix::Disk(_))
        )
}

async fn exe_file_version(path: &str) -> Option<String> {
    if !is_local_drive_path(path) {
        return None;
    }
    let script = format!(
        "try {{ (Get-Item -LiteralPath '{}').VersionInfo.ProductVersion }} catch {{ '' }}",
        path.replace('\'', "''")
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", &script]);
    let (_code, out) = proc::run_capped_cmd(cmd, VERIFY).await;
    let v = out.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_local_drive_path_rejects_unc_and_relative() {
        assert!(is_local_drive_path("C:\\Program Files\\App\\app.exe"));
        assert!(is_local_drive_path("D:\\x.exe"));
        // UNC would force LocalSystem to authenticate to a remote SMB host.
        assert!(!is_local_drive_path("\\\\attacker.example\\share\\x.exe"));
        assert!(!is_local_drive_path("\\\\?\\UNC\\host\\share\\x.exe"));
        // Relative / bare paths.
        assert!(!is_local_drive_path("app.exe"));
        assert!(!is_local_drive_path("..\\x.exe"));
    }

    #[test]
    fn only_a_mismatch_is_re_read() {
        // A just-installed app can still report the old version for a moment, so that
        // one verdict gets the second read; everything else settles on the first.
        assert!(!settled(Verification::Mismatch));
        assert!(settled(Verification::Verified));
        assert!(settled(Verification::Unverified));
        assert!(settled(Verification::NotChecked));
    }

    #[test]
    fn exe_fallback_softens_only_a_real_mismatch() {
        // An exe-fallback mismatch (4-part FILEVERSION below marketing) is softened…
        assert_eq!(
            soften_exe_fallback(Verification::Mismatch, true),
            Verification::Unverified
        );
        // …but a winget (non-exe) mismatch stays a mismatch (the update didn't take).
        assert_eq!(
            soften_exe_fallback(Verification::Mismatch, false),
            Verification::Mismatch
        );
        // Verified/Unverified are untouched regardless of source.
        assert_eq!(
            soften_exe_fallback(Verification::Verified, true),
            Verification::Verified
        );
        assert_eq!(
            soften_exe_fallback(Verification::Unverified, true),
            Verification::Unverified
        );
    }
}
