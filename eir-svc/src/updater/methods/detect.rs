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

/// Package-family hash for the official winget (Microsoft.DesktopAppInstaller) MSIX.
const WINGET_FAMILY: &str = "__8wekyb3d8bbwe";

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

/// winget is present on modern Windows. Checks the interactive PATH alias first,
/// then resolves the real MSIX exe under `C:\Program Files\WindowsApps` — the latter
/// is needed when EirSvc runs as LocalSystem, whose profile has no winget alias.
pub fn winget_available() -> bool {
    winget_path().is_some()
}

/// Absolute path to `winget.exe`, or `None` if it cannot be found. Prefer the PATH
/// alias when present, else enumerate the system WindowsApps package directory.
pub(crate) fn winget_path() -> Option<PathBuf> {
    resolve_winget_from_path().or_else(resolve_winget_from_windowsapps)
}

fn resolve_winget_from_path() -> Option<PathBuf> {
    let out = std::process::Command::new("where")
        .arg("winget")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn resolve_winget_from_windowsapps() -> Option<PathBuf> {
    let root = PathBuf::from("C:\\Program Files\\WindowsApps");
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("Microsoft.DesktopAppInstaller_") || !name.ends_with(WINGET_FAMILY) {
            continue;
        }
        let exe = entry.path().join("winget.exe");
        if !exe.is_file() {
            continue;
        }
        let version = extract_winget_dir_version(&name)?;
        let better = best.as_ref().is_none_or(|(v, _)| {
            crate::updater::version::version_cmp(&version, v)
                .is_some_and(|o| o == std::cmp::Ordering::Greater)
        });
        if better {
            best = Some((version, exe));
        }
    }
    best.map(|(_, p)| p)
}

/// `Microsoft.DesktopAppInstaller_<version>_<arch>__8wekyb3d8bbwe` -> `<version>`.
fn extract_winget_dir_version(dir_name: &str) -> Option<String> {
    let body = dir_name
        .strip_prefix("Microsoft.DesktopAppInstaller_")?
        .strip_suffix(WINGET_FAMILY)?;
    // Body is "<Version>_<Architecture>" (version never contains underscores).
    let (version, _arch) = body.rsplit_once('_')?;
    Some(version.to_string())
}

/// Install Chocolatey via its official bootstrap script (runs as SYSTEM, no UAC).
/// Returns true only if choco.exe is present afterwards.
pub async fn bootstrap_choco() -> bool {
    info!("Chocolatey not found — bootstrapping it");
    const SCRIPT: &str = "Set-ExecutionPolicy Bypass -Scope Process -Force; \
         [System.Net.ServicePointManager]::SecurityProtocol = 3072; \
         iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))";
    // Bounded, like every other external command in the updater: a stalled download or
    // hung install script must NOT wedge the cycle (which would latch "running" forever).
    // The bootstrap is a network install, so it gets the INSTALL budget rather than LIST.
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        SCRIPT,
    ]);
    let (code, _out) =
        crate::updater::proc::run_capped_cmd(cmd, crate::updater::proc::INSTALL).await;
    let ok = code == 0 && choco_available();
    if !ok {
        warn!("Chocolatey bootstrap did not complete (exit {code})");
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_winget_dir_version_parses_stable_dirs() {
        assert_eq!(
            extract_winget_dir_version(
                "Microsoft.DesktopAppInstaller_1.25.340.0_x64__8wekyb3d8bbwe"
            ),
            Some("1.25.340.0".to_string())
        );
        assert_eq!(
            extract_winget_dir_version(
                "Microsoft.DesktopAppInstaller_1.10.0.0_neutral__8wekyb3d8bbwe"
            ),
            Some("1.10.0.0".to_string())
        );
    }

    #[test]
    fn extract_winget_dir_version_rejects_other_packages() {
        assert!(
            extract_winget_dir_version("Microsoft.WindowsStore_123_x64__8wekyb3d8bbwe").is_none()
        );
        assert!(extract_winget_dir_version("not_a_package").is_none());
    }
}
