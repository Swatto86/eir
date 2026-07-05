//! Game Mode auto-detection. Polls whether a fullscreen game/app is running and reports it
//! to the service (over the pipe) so Eir quiets its background work while you play. This
//! runs in the tray (the interactive user's session) because the LocalSystem service
//! (session 0) can't see the desktop's fullscreen state.

use crate::pipe_client::SharedStatus;
use eir_proto::UiMsg;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN,
};

const POLL: Duration = Duration::from_secs(5);
/// Re-assert the auto lease this often while a game runs (the service lease is 90s).
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Require this much sustained non-fullscreen before leaving Game Mode, so a quick
/// alt-tab doesn't kick off an updater cycle.
const EXIT_DEBOUNCE: Duration = Duration::from_secs(60);

/// True if a fullscreen (game/exclusive) app is foreground. `SHQueryUserNotificationState`
/// is the OS's own "should I show toasts?" signal: `QUNS_RUNNING_D3D_FULL_SCREEN` is a D3D
/// game, `QUNS_BUSY` is a fullscreen app generally — either means "don't interrupt."
fn is_fullscreen_app() -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_BUSY,
        Err(_) => false,
    }
}

/// The auto-detect loop. Only acts while the pipe is connected and the `game_mode_auto`
/// setting is on; sends `SetGaming { manual: false }` (the heartbeat lease) on transitions.
pub async fn run(status: SharedStatus, cmd_tx: Sender<UiMsg>, connected: Arc<AtomicBool>) {
    let mut gaming = false; // our last auto-reported state
    let mut non_fs_since: Option<Instant> = None;
    let mut last_heartbeat = Instant::now();
    let mut cleared_stale = false;

    loop {
        tokio::time::sleep(POLL).await;
        if !connected.load(Ordering::Relaxed) {
            continue;
        }
        // On first connect, clear any stale auto-lease/power a prior tray instance may have
        // left (covers a tray relaunch after a crash). Harmless if nothing was set.
        if !cleared_stale {
            let _ = cmd_tx.try_send(UiMsg::SetGaming {
                on: false,
                manual: false,
            });
            cleared_stale = true;
            gaming = false;
        }
        // Use the module's poison-safe lock helper (recovers a poisoned mutex) rather than
        // `.lock().ok()`, which would silently disable auto-detect forever if the status
        // mutex were ever poisoned by an unrelated panic.
        let auto = crate::pipe_client::lock_status(&status)
            .settings
            .as_ref()
            .map(|x| x.game_mode_auto)
            .unwrap_or(false);
        if !auto {
            // Auto disabled: withdraw any auto lease we set (the manual toggle is separate).
            if gaming {
                let _ = cmd_tx.try_send(UiMsg::SetGaming {
                    on: false,
                    manual: false,
                });
                gaming = false;
                non_fs_since = None;
            }
            continue;
        }
        if is_fullscreen_app() {
            non_fs_since = None;
            if !gaming || last_heartbeat.elapsed() >= HEARTBEAT {
                let _ = cmd_tx.try_send(UiMsg::SetGaming {
                    on: true,
                    manual: false,
                });
                last_heartbeat = Instant::now();
                gaming = true;
            }
        } else if gaming {
            let since = *non_fs_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= EXIT_DEBOUNCE {
                let _ = cmd_tx.try_send(UiMsg::SetGaming {
                    on: false,
                    manual: false,
                });
                gaming = false;
                non_fs_since = None;
            }
        }
    }
}
