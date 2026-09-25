//! Service start/stop/restart. The mechanism is platform-specific (Windows SCM API /
//! Linux `systemctl`); `restart()` (stop then start) is the one shape both share.

use anyhow::Result;
use tracing::info;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{start, stop};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{start, stop};

pub fn restart(name: &str) -> Result<String> {
    info!(service = name, "Restarting service");
    stop(name)?;
    start(name)?;
    Ok(format!("Service '{name}' restarted successfully"))
}
