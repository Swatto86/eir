//! Package-manager detection and bootstrap. Decides which methods are usable on
//! this machine right now and, when allowed, installs a missing manager.
//!
//! Context notes that matter for the unattended SYSTEM service:
//!   - winget and Chocolatey run fine as SYSTEM/admin.
//!   - Scoop is user-scoped and its shim is user-writable. The SYSTEM service must
//!     neither discover nor execute it; Scoop stays unavailable until it has a real
//!     active-user process launcher.
//!   - Only Chocolatey is auto-bootstrapped.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::OnceLock;
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

pub async fn choco_path_async() -> Option<PathBuf> {
    tokio::task::spawn_blocking(choco_path)
        .await
        .unwrap_or_else(|error| {
            warn!("Chocolatey path lookup task failed: {error}");
            None
        })
}

pub async fn choco_available_async() -> bool {
    choco_path_async().await.is_some()
}

/// Scoop shims live under user-writable profiles. Selecting one from the SYSTEM
/// service would turn a normal user-owned `.cmd` into arbitrary SYSTEM code.
pub fn scoop_available() -> bool {
    false
}

#[derive(Clone)]
struct WingetResolution {
    path: Option<PathBuf>,
    reason: Option<String>,
}

static WINGET_RESOLUTION: OnceLock<WingetResolution> = OnceLock::new();

/// winget is present on modern Windows. Checks the interactive PATH alias first,
/// then resolves the real MSIX exe under `C:\Program Files\WindowsApps` — the latter
/// is needed when EirSvc runs as LocalSystem, whose profile has no winget alias.
pub async fn winget_available_async() -> bool {
    winget_path_async().await.is_some()
}

/// Absolute path to `winget.exe`, or `None` if it cannot be found. Prefer the PATH
/// alias when present, else enumerate the system WindowsApps package directory, and
/// finally fall back to the AppX package repository via PowerShell (some SYSTEM
/// profiles can query the package but cannot list the WindowsApps folder).
pub(crate) fn winget_path() -> Option<PathBuf> {
    WINGET_RESOLUTION.get_or_init(resolve_winget).path.clone()
}

pub(crate) async fn winget_path_async() -> Option<PathBuf> {
    tokio::task::spawn_blocking(winget_path)
        .await
        .unwrap_or_else(|error| {
            warn!("winget path lookup task failed: {error}");
            None
        })
}

/// Human-readable reason winget could not be resolved, or `None` if it is available.
/// Surfaced in the updater note so a "winget not available" message is provably
/// either a stale service binary (pre-resolution code) or a genuine SYSTEM-level
/// failure.
pub fn winget_unavailability_reason() -> Option<String> {
    WINGET_RESOLUTION.get_or_init(resolve_winget).reason.clone()
}

pub async fn winget_unavailability_reason_async() -> Option<String> {
    tokio::task::spawn_blocking(winget_unavailability_reason)
        .await
        .unwrap_or_else(|error| {
            warn!("winget availability lookup task failed: {error}");
            Some("winget availability lookup failed".to_string())
        })
}

fn resolve_winget() -> WingetResolution {
    let mut reasons: Vec<String> = Vec::new();

    match resolve_winget_from_path() {
        Ok(Some(p)) => {
            info!(path = %p.display(), "winget resolved from PATH");
            return WingetResolution {
                path: Some(p),
                reason: None,
            };
        }
        Ok(None) => reasons.push("not found in PATH".to_string()),
        Err(e) => reasons.push(format!("PATH lookup failed: {e}")),
    }
    warn!("winget not found in PATH; trying WindowsApps directory");

    match resolve_winget_from_windowsapps() {
        Ok(Some(p)) => {
            info!(path = %p.display(), "winget resolved from WindowsApps directory");
            return WingetResolution {
                path: Some(p),
                reason: None,
            };
        }
        Ok(None) => reasons.push(
            "no Microsoft.DesktopAppInstaller package under C:\\Program Files\\WindowsApps"
                .to_string(),
        ),
        Err(e) => reasons.push(format!("WindowsApps scan failed: {e}")),
    }
    warn!("winget not found in WindowsApps; trying AppX package repository");

    match resolve_winget_from_appx() {
        Ok(Some(p)) => {
            info!(path = %p.display(), "winget resolved from AppX package repository");
            return WingetResolution {
                path: Some(p),
                reason: None,
            };
        }
        Ok(None) => reasons.push(
            "Microsoft.DesktopAppInstaller not registered in the AppX package repository"
                .to_string(),
        ),
        Err(e) => reasons.push(format!("AppX repository query failed: {e}")),
    }

    warn!("winget could not be resolved; updates will use registry inventory only");
    let reason = format!("winget unavailable — {}", reasons.join("; "));
    WingetResolution {
        path: None,
        reason: Some(reason),
    }
}

fn resolve_winget_from_path() -> Result<Option<PathBuf>, String> {
    let out = std::process::Command::new("where")
        .arg("winget")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .find(|p| p.is_file()))
}

fn resolve_winget_from_windowsapps() -> Result<Option<PathBuf>, String> {
    let root = PathBuf::from("C:\\Program Files\\WindowsApps");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) => return Err(e.to_string()),
    };
    let mut best: Option<(String, PathBuf)> = None;
    let mut found = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("Microsoft.DesktopAppInstaller_") || !name.ends_with(WINGET_FAMILY) {
            continue;
        }
        found += 1;
        let exe = entry.path().join("winget.exe");
        if !exe.is_file() {
            continue;
        }
        let version = match extract_winget_dir_version(&name) {
            Some(v) => v,
            None => {
                warn!(dir = %name, "could not parse winget directory version");
                continue;
            }
        };
        let better = best.as_ref().is_none_or(|(v, _)| {
            crate::updater::version::version_cmp(&version, v)
                .is_some_and(|o| o == std::cmp::Ordering::Greater)
        });
        if better {
            best = Some((version, exe));
        }
    }
    if found == 0 {
        warn!(root = %root.display(), "no Microsoft.DesktopAppInstaller package found in WindowsApps");
    }
    Ok(best.map(|(_, p)| p))
}

fn resolve_winget_from_appx() -> Result<Option<PathBuf>, String> {
    const SCRIPT: &str = "Get-AppxPackage -AllUsers | Where-Object { $_.Name -eq 'Microsoft.DesktopAppInstaller' } | Select-Object -First 1 -ExpandProperty InstallLocation";
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "Get-AppxPackage query failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let loc = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if loc.is_empty() {
        return Ok(None);
    }
    let exe = PathBuf::from(loc).join("winget.exe");
    if exe.is_file() {
        Ok(Some(exe))
    } else {
        Err(format!(
            "AppX install location does not contain winget.exe: {}",
            exe.display()
        ))
    }
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
    let ok = code == 0 && choco_available_async().await;
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

    #[test]
    fn scoop_is_never_selected_from_a_user_profile_by_the_system_service() {
        assert!(!scoop_available());
    }

    #[tokio::test]
    async fn async_manager_resolution_matches_the_cached_sync_result() {
        // If winget resolves, there must be no unavailability reason; if it does not,
        // the reason must explain why. This guards the cache staying internally
        // consistent regardless of the machine state.
        let winget = winget_path_async().await;
        assert_eq!(winget, winget_path());
        assert_eq!(choco_path_async().await, choco_path());
        if winget.is_some() {
            assert!(
                winget_unavailability_reason().is_none(),
                "winget resolved but a reason was also recorded"
            );
        } else {
            let reason =
                winget_unavailability_reason().expect("unavailable winget should record a reason");
            assert!(!reason.is_empty());
        }
    }
}
