//! Package-manager detection and bootstrap. Decides which methods are usable on
//! this machine right now and, when allowed, installs a missing manager.
//!
//! Context notes that matter for the unattended SYSTEM service:
//!   - winget and Chocolatey run fine as SYSTEM/admin.
//!   - Scoop is user-scoped; the service borrows the logged-in user's install
//!     (like it borrows their Claude session) and runs scoop in that profile's
//!     context — best-effort.
//!   - Only Chocolatey is auto-bootstrapped; installing Scoop as SYSTEM would create
//!     a SYSTEM-profile scoop nobody uses, so we never do that.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use tracing::{info, warn};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Path to `choco.exe`, if Chocolatey is installed (machine-wide default location).
pub fn choco_path() -> Option<PathBuf> {
    let pd = std::env::var("ProgramData").ok()?;
    let p = PathBuf::from(pd)
        .join("chocolatey")
        .join("bin")
        .join("choco.exe");
    p.is_file().then_some(p)
}

pub fn choco_available() -> bool {
    choco_path().is_some()
}

/// Find a logged-in user's Scoop install. Returns (user_profile_root, scoop.cmd).
pub fn scoop_install() -> Option<(String, PathBuf)> {
    let users = std::fs::read_dir("C:\\Users").ok()?;
    for entry in users.flatten() {
        let dir = entry.path();
        let shim = dir.join("scoop").join("shims").join("scoop.cmd");
        if shim.is_file() {
            return Some((dir.to_string_lossy().into_owned(), shim));
        }
    }
    None
}

pub fn scoop_available() -> bool {
    scoop_install().is_some()
}

/// winget is present on modern Windows; confirm via `where`.
pub fn winget_available() -> bool {
    std::process::Command::new("where")
        .arg("winget")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Install Chocolatey via its official bootstrap script (runs as SYSTEM, no UAC).
/// Returns true only if choco.exe is present afterwards.
pub async fn bootstrap_choco() -> bool {
    info!("Chocolatey not found — bootstrapping it");
    const SCRIPT: &str = "Set-ExecutionPolicy Bypass -Scope Process -Force; \
         [System.Net.ServicePointManager]::SecurityProtocol = 3072; \
         iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))";
    let ran = tokio::task::spawn_blocking(|| {
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                SCRIPT,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await;
    let ok = matches!(ran, Ok(Ok(s)) if s.success()) && choco_available();
    if !ok {
        warn!("Chocolatey bootstrap did not complete");
    }
    ok
}
