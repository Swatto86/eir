//! Game Mode auto-detection. Polls whether a fullscreen game/app is running and reports it
//! to the service (over the pipe) so Eir quiets its background work while you play. This
//! runs in the tray (the interactive user's session) because the LocalSystem service
//! (session 0) can't see the desktop's fullscreen state.

use crate::pipe_client::SharedStatus;
use eir_proto::{UiMsg, UiRequest};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUERY_USER_NOTIFICATION_STATE, QUNS_BUSY,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const POLL: Duration = Duration::from_secs(5);
/// Re-assert the auto lease this often while a game runs (the service lease is 90s).
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Require this much sustained non-fullscreen before leaving Game Mode, so a quick
/// alt-tab doesn't kick off an updater cycle.
const EXIT_DEBOUNCE: Duration = Duration::from_secs(60);

/// True if a fullscreen (game/exclusive) app is foreground. `SHQueryUserNotificationState`
/// is the OS's own "should I show toasts?" signal: `QUNS_RUNNING_D3D_FULL_SCREEN` is a D3D
/// game, `QUNS_BUSY` is a fullscreen app generally — either means "don't interrupt."
fn notification_state_is_game(
    state: QUERY_USER_NOTIFICATION_STATE,
    foreground_process_id: Option<u32>,
    own_process_id: u32,
) -> bool {
    (state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_BUSY)
        && foreground_process_id != Some(own_process_id)
}

fn foreground_process_id() -> Option<u32> {
    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut process_id));
    }
    (process_id != 0).then_some(process_id)
}

fn is_fullscreen_app() -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => notification_state_is_game(state, foreground_process_id(), std::process::id()),
        Err(_) => false,
    }
}

fn try_set_gaming(cmd_tx: &Sender<UiRequest>, on: bool) -> bool {
    cmd_tx
        .try_send(UiMsg::SetGaming { on, manual: false }.into())
        .is_ok()
}

/// The auto-detect loop. Only acts while the pipe is connected and the `game_mode_auto`
/// setting is on; sends `SetGaming { manual: false }` (the heartbeat lease) on transitions.
pub async fn run(status: SharedStatus, cmd_tx: Sender<UiRequest>, connected: Arc<AtomicBool>) {
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
        if !cleared_stale && try_set_gaming(&cmd_tx, false) {
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
            if gaming && try_set_gaming(&cmd_tx, false) {
                gaming = false;
                non_fs_since = None;
            }
            continue;
        }
        if is_fullscreen_app() {
            non_fs_since = None;
            if (!gaming || last_heartbeat.elapsed() >= HEARTBEAT) && try_set_gaming(&cmd_tx, true) {
                last_heartbeat = Instant::now();
                gaming = true;
            }
        } else if gaming {
            let since = *non_fs_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= EXIT_DEBOUNCE && try_set_gaming(&cmd_tx, false) {
                gaming = false;
                non_fs_since = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_state_ignores_eirs_own_foreground_window() {
        assert!(!notification_state_is_game(QUNS_BUSY, Some(42), 42));
        assert!(notification_state_is_game(QUNS_BUSY, Some(7), 42));
        assert!(notification_state_is_game(
            QUNS_RUNNING_D3D_FULL_SCREEN,
            None,
            42
        ));
    }

    #[test]
    fn full_queue_reports_unsent_transition() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(try_set_gaming(&tx, true));
        assert!(!try_set_gaming(&tx, false));
    }
}
