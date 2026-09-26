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
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUERY_USER_NOTIFICATION_STATE, QUNS_BUSY,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const POLL: Duration = Duration::from_secs(5);
/// Re-assert the auto lease this often while a game runs (the service lease is 90s).
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Require this much sustained non-fullscreen before leaving Game Mode once the game has
/// closed (or cannot be seen), so a quick switch doesn't kick off an updater cycle.
const EXIT_DEBOUNCE: Duration = Duration::from_secs(60);
/// How long Game Mode holds while the game is still running but not in front (a break to
/// Discord or a browser). Leaving Game Mode restores the power plan and lets a deferred
/// analysis or update start, so with the 60-second rule alone it dropped and returned
/// several times an hour during play, starting AI runs mid-session.
const AWAY_GRACE: Duration = Duration::from_secs(10 * 60);

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

/// `Some(foreground process id, if known)` while a fullscreen app is in front.
fn fullscreen_app() -> Option<Option<u32>> {
    let foreground = foreground_process_id();
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => {
            notification_state_is_game(state, foreground, std::process::id()).then_some(foreground)
        }
        Err(_) => None,
    }
}

/// The process that was fullscreen, held open so its exit can be seen and its id is never
/// reused while held. The handle is kept as an integer so the detector future stays `Send`.
struct GameProcess {
    pid: u32,
    handle: isize,
}

impl GameProcess {
    fn open(pid: u32) -> Option<Self> {
        // An elevated or protected game refuses this; the 60-second exit rule then applies.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.ok()?;
        Some(Self {
            pid,
            handle: handle.0 as isize,
        })
    }

    fn running(&self) -> bool {
        let state = unsafe { WaitForSingleObject(HANDLE(self.handle as *mut _), 0) };
        state == WAIT_TIMEOUT
    }
}

impl Drop for GameProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(HANDLE(self.handle as *mut _));
        }
    }
}

/// Game Mode's on/off decisions. Pure, so the timing rules are testable.
#[derive(Debug, Default)]
struct Detector {
    gaming: bool,
    away_since: Option<Instant>,
    last_heartbeat: Option<Instant>,
}

impl Detector {
    /// The report due after a poll at `now`: `Some(true)` starts or re-asserts Game Mode
    /// (the service lease is 90 s), `Some(false)` ends it, `None` changes nothing.
    fn report(&self, now: Instant, fullscreen: bool, game_running: bool) -> Option<bool> {
        let heartbeat_due = self
            .last_heartbeat
            .is_none_or(|at| now.saturating_duration_since(at) >= HEARTBEAT);
        if fullscreen {
            return (!self.gaming || heartbeat_due).then_some(true);
        }
        if !self.gaming {
            return None;
        }
        let away = self
            .away_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let limit = if game_running {
            AWAY_GRACE
        } else {
            EXIT_DEBOUNCE
        };
        if away >= limit {
            Some(false)
        } else {
            heartbeat_due.then_some(true)
        }
    }

    /// Record a poll: `sent` is the report that reached the service, if any.
    fn observe(&mut self, now: Instant, fullscreen: bool, sent: Option<bool>) {
        if fullscreen {
            self.away_since = None;
        } else if self.gaming && self.away_since.is_none() {
            self.away_since = Some(now);
        }
        match sent {
            Some(true) => {
                self.gaming = true;
                self.last_heartbeat = Some(now);
            }
            Some(false) => *self = Self::default(),
            None => {}
        }
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
    let mut detector = Detector::default();
    let mut game: Option<GameProcess> = None;
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
            detector = Detector::default();
            game = None;
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
            if detector.gaming && try_set_gaming(&cmd_tx, false) {
                detector = Detector::default();
                game = None;
            }
            continue;
        }
        let now = Instant::now();
        let fullscreen = match fullscreen_app() {
            Some(foreground) => {
                if foreground.is_some() && game.as_ref().map(|g| g.pid) != foreground {
                    game = foreground.and_then(GameProcess::open);
                }
                true
            }
            None => false,
        };
        let game_running = game.as_ref().is_some_and(GameProcess::running);
        let sent = detector
            .report(now, fullscreen, game_running)
            .filter(|&on| try_set_gaming(&cmd_tx, on));
        detector.observe(now, fullscreen, sent);
        if !detector.gaming {
            game = None;
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

    /// Poll every POLL from `start` for `span`, sending whatever is due; returns the
    /// reports sent.
    fn play(
        detector: &mut Detector,
        start: Instant,
        span: Duration,
        fullscreen: bool,
        game_running: bool,
    ) -> Vec<bool> {
        let mut sent = Vec::new();
        let mut at = start;
        while at <= start + span {
            let report = detector.report(at, fullscreen, game_running);
            detector.observe(at, fullscreen, report);
            sent.extend(report);
            at += POLL;
        }
        sent
    }

    #[test]
    fn a_fullscreen_game_starts_game_mode_and_keeps_its_lease_alive() {
        let mut detector = Detector::default();
        let start = Instant::now();
        let sent = play(&mut detector, start, Duration::from_secs(60), true, true);
        assert_eq!(
            sent,
            [true, true, true],
            "start, then a heartbeat every 30 s"
        );
        assert!(detector.gaming);
    }

    #[test]
    fn a_break_from_a_running_game_keeps_game_mode_for_the_grace_period() {
        // The live bug: alt-tabbing out of WoW for over a minute ended Game Mode, which
        // restored the power plan and started a deferred AI analysis mid-session.
        let mut detector = Detector::default();
        let start = Instant::now();
        play(&mut detector, start, Duration::ZERO, true, true);
        let away = start + POLL;
        let sent = play(&mut detector, away, AWAY_GRACE - POLL * 2, false, true);
        assert!(
            sent.iter().all(|&on| on),
            "only heartbeats during the break"
        );
        assert!(detector.gaming);
        let sent = play(
            &mut detector,
            away + AWAY_GRACE,
            Duration::ZERO,
            false,
            true,
        );
        assert_eq!(
            sent,
            [false],
            "a break longer than the grace period ends it"
        );
        assert!(!detector.gaming);
    }

    #[test]
    fn closing_the_game_ends_game_mode_after_a_minute() {
        let mut detector = Detector::default();
        let start = Instant::now();
        play(&mut detector, start, Duration::ZERO, true, true);
        let closed = start + POLL;
        assert!(
            play(&mut detector, closed, EXIT_DEBOUNCE - POLL, false, false)
                .iter()
                .all(|&on| on)
        );
        assert!(detector.gaming);
        assert_eq!(
            play(
                &mut detector,
                closed + EXIT_DEBOUNCE,
                Duration::ZERO,
                false,
                false
            ),
            [false]
        );
    }

    #[test]
    fn returning_to_the_game_resets_the_break() {
        let mut detector = Detector::default();
        let start = Instant::now();
        play(&mut detector, start, Duration::ZERO, true, true);
        play(
            &mut detector,
            start + POLL,
            Duration::from_secs(50),
            false,
            false,
        );
        play(
            &mut detector,
            start + Duration::from_secs(60),
            Duration::ZERO,
            true,
            true,
        );
        assert!(detector.away_since.is_none());
        let sent = play(
            &mut detector,
            start + Duration::from_secs(65),
            Duration::from_secs(50),
            false,
            false,
        );
        assert!(
            sent.iter().all(|&on| on),
            "a fresh break starts its own minute"
        );
        assert!(detector.gaming);
    }

    #[test]
    fn a_held_game_process_reads_running_until_it_exits() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let game = GameProcess::open(child.id()).unwrap();
        assert!(game.running());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!game.running());
    }

    #[test]
    fn an_unsent_report_changes_nothing() {
        let mut detector = Detector::default();
        let now = Instant::now();
        assert_eq!(detector.report(now, true, true), Some(true));
        detector.observe(now, true, None);
        assert!(
            !detector.gaming,
            "a full queue must not count as Game Mode on"
        );
        assert_eq!(detector.report(now + POLL, true, true), Some(true));
    }
}
