//! On-screen error watcher. Every couple of seconds it looks for classic error message
//! boxes and windows that have stopped responding, and reports each one once to the
//! service (`UiMsg::ReportScreenError`), which treats it as a signal and reacts. It runs
//! in the tray because the LocalSystem service (session 0) cannot see the desktop.
//!
//! Coverage is deliberately narrow: standard dialogs (`#32770`) shaped like a message box
//! whose text reads like an error, and visible top-level windows Windows itself considers
//! hung (`IsHungAppWindow`, no input processed for 5 s) on two consecutive polls.

use crate::pipe_client::SharedStatus;
use eir_proto::{UiMsg, UiRequest, CAP_SCREEN_ERRORS};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId,
    IsHungAppWindow, IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    WM_GETTEXT,
};

const POLL: Duration = Duration::from_secs(2);
/// A message box has an icon, its text and a few buttons; property sheets and other
/// dialogs full of labels (which can mention "error checking" etc.) have many more.
const MAX_MESSAGE_BOX_CHILDREN: usize = 10;
const MAX_TEXT_CHARS: usize = 1500;
const ERROR_WORDS: &[&str] = &[
    "error",
    "fail",
    "exception",
    "crash",
    "fatal",
    "cannot",
    "can't",
    "could not",
    "couldn't",
    "unable",
    "stopped working",
    "not responding",
    "corrupt",
    "access is denied",
    "access denied",
    "not found",
    "missing",
    "invalid",
    "0x8",
    "0xc0",
];

/// True when a dialog's title/text reads like an error (pure, unit-tested).
pub fn looks_like_error(title: &str, text: &str) -> bool {
    let haystack = format!("{title}\n{text}").to_lowercase();
    ERROR_WORDS.iter().any(|w| haystack.contains(w))
}

/// One window of interest seen on a poll.
#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    Dialog {
        hwnd: isize,
        app: String,
        title: String,
        text: String,
    },
    Hung {
        hwnd: isize,
        app: String,
        title: String,
    },
}

/// What to send to the service.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub app: String,
    pub title: String,
    pub text: String,
    pub hung: bool,
}

/// Remembers which windows were already reported, and which looked hung on the previous
/// poll, so each dialog or hang is reported exactly once while it stays on screen.
#[derive(Default)]
pub struct Tracker {
    reported: HashSet<isize>,
    hung_candidates: HashSet<isize>,
}

impl Tracker {
    pub fn step(&mut self, observations: Vec<Observation>) -> Vec<Report> {
        let mut reports = Vec::new();
        let mut present = HashSet::new();
        let mut hung_now = HashSet::new();
        for o in observations {
            match o {
                Observation::Dialog {
                    hwnd,
                    app,
                    title,
                    text,
                } => {
                    present.insert(hwnd);
                    if looks_like_error(&title, &text) && self.reported.insert(hwnd) {
                        reports.push(Report {
                            app,
                            title,
                            text,
                            hung: false,
                        });
                    }
                }
                Observation::Hung { hwnd, app, title } => {
                    present.insert(hwnd);
                    hung_now.insert(hwnd);
                    // Report only a hang that lasted across two polls, once.
                    if self.hung_candidates.contains(&hwnd) && self.reported.insert(hwnd) {
                        reports.push(Report {
                            app,
                            title,
                            text: String::new(),
                            hung: true,
                        });
                    }
                }
            }
        }
        // A closed window (or one that recovered) can be reported again if it returns.
        self.reported.retain(|h| present.contains(h));
        self.hung_candidates = hung_now;
        reports
    }
}

fn utf16(buf: &[u16], len: usize) -> String {
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: the buffer is a live stack array and its length is passed implicitly.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    utf16(&buf, usize::try_from(n).unwrap_or(0))
}

/// Title of a top-level window. Cross-process, this reads the caption the system stores
/// without sending the window a message, so a hung window cannot block the watcher.
fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: as above.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    utf16(&buf, usize::try_from(n).unwrap_or(0))
}

/// Text of a control in another process (WM_GETTEXT, abandoned after 250 ms if the owner
/// is not responding).
fn control_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 2048];
    let mut copied = 0usize;
    // SAFETY: WM_GETTEXT writes at most `wparam` UTF-16 units into the live buffer.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            250,
            Some(&mut copied),
        )
    };
    if ok.0 == 0 {
        return String::new();
    }
    utf16(&buf, copied)
}

fn process_name(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed below; the buffer outlives the call.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).ok()?;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = utf16(&buf, len as usize);
        path.rsplit('\\').next().map(str::to_string)
    }
}

unsafe extern "system" fn collect_hwnd(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` passed by the enumerating caller, which
    // outlives the synchronous enumeration.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    TRUE
}

fn top_level_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `list`, which lives across the call.
    let _ = unsafe {
        EnumWindows(
            Some(collect_hwnd),
            LPARAM(std::ptr::from_mut(&mut list) as isize),
        )
    };
    list
}

fn children(hwnd: HWND) -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: as for `top_level_windows`.
    let _ = unsafe {
        EnumChildWindows(
            hwnd,
            Some(collect_hwnd),
            LPARAM(std::ptr::from_mut(&mut list) as isize),
        )
    };
    list
}

/// Read one message-box-shaped dialog, or `None` for any other dialog.
fn read_dialog(hwnd: HWND) -> Option<String> {
    let kids = children(hwnd);
    if kids.is_empty() || kids.len() > MAX_MESSAGE_BOX_CHILDREN {
        return None;
    }
    let mut has_button = false;
    let mut text = String::new();
    for kid in kids {
        match class_name(kid).as_str() {
            "Button" => has_button = true,
            "Static" => {
                let t = control_text(kid);
                let t = t.trim();
                if !t.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            _ => {}
        }
    }
    if !has_button {
        return None;
    }
    Some(text.chars().take(MAX_TEXT_CHARS).collect())
}

/// Blocking Win32 enumeration — call from a blocking task.
pub fn observe(own_pid: u32) -> Vec<Observation> {
    let mut out = Vec::new();
    for hwnd in top_level_windows() {
        // SAFETY: plain queries on a window handle; a stale handle just returns false.
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            continue;
        }
        let mut pid = 0u32;
        // SAFETY: `pid` is a live u32.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 || pid == own_pid {
            continue;
        }
        let class = class_name(hwnd);
        let key = hwnd.0 as isize;
        // SAFETY: a plain query on a window handle.
        let hung = unsafe { IsHungAppWindow(hwnd) }.as_bool();
        if hung && class != "Ghost" {
            let title = window_title(hwnd);
            if !title.trim().is_empty() {
                if let Some(app) = process_name(pid) {
                    out.push(Observation::Hung {
                        hwnd: key,
                        app,
                        title,
                    });
                }
            }
            continue;
        }
        if class == "#32770" {
            if let (Some(text), Some(app)) = (read_dialog(hwnd), process_name(pid)) {
                out.push(Observation::Dialog {
                    hwnd: key,
                    app,
                    title: window_title(hwnd),
                    text,
                });
            }
        }
    }
    out
}

fn enabled(status: &SharedStatus) -> bool {
    let s = crate::pipe_client::lock_status(status);
    s.capabilities.iter().any(|c| c == CAP_SCREEN_ERRORS)
        && s.settings.as_ref().is_some_and(|x| x.watch_screen_errors)
}

/// The watcher loop. Only reports while the pipe is connected, the service supports the
/// message, and the user has the setting on.
pub async fn run(status: SharedStatus, cmd_tx: Sender<UiRequest>, connected: Arc<AtomicBool>) {
    let own_pid = std::process::id();
    let mut tracker = Tracker::default();
    loop {
        tokio::time::sleep(POLL).await;
        if !connected.load(Ordering::Relaxed) || !enabled(&status) {
            continue;
        }
        let observations =
            match tauri::async_runtime::spawn_blocking(move || observe(own_pid)).await {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("screen watcher enumeration failed: {e}");
                    continue;
                }
            };
        for r in tracker.step(observations) {
            let msg = UiMsg::ReportScreenError {
                app: r.app,
                title: r.title,
                text: r.text,
                hung: r.hung,
            };
            if let Err(e) = cmd_tx.try_send(msg.into()) {
                eprintln!("screen watcher could not queue a report: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(hwnd: isize, title: &str, text: &str) -> Observation {
        Observation::Dialog {
            hwnd,
            app: "app.exe".into(),
            title: title.into(),
            text: text.into(),
        }
    }

    fn hung(hwnd: isize) -> Observation {
        Observation::Hung {
            hwnd,
            app: "word.exe".into(),
            title: "Report.docx - Word".into(),
        }
    }

    #[test]
    fn error_wording_is_recognised_and_ordinary_prompts_are_not() {
        assert!(looks_like_error(
            "Microsoft Outlook",
            "Cannot start Microsoft Outlook."
        ));
        assert!(looks_like_error("Application Error", ""));
        assert!(looks_like_error(
            "Setup",
            "Installation failed with 0x80070005"
        ));
        assert!(!looks_like_error(
            "Notepad",
            "Do you want to save changes to Untitled?"
        ));
        assert!(!looks_like_error(
            "Confirm Save As",
            "report.txt already exists."
        ));
    }

    #[test]
    fn each_error_dialog_is_reported_once_while_it_stays_open() {
        let mut t = Tracker::default();
        let first = t.step(vec![
            dialog(1, "App", "Fatal error: out of memory"),
            dialog(2, "Notepad", "Save changes?"),
        ]);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].text, "Fatal error: out of memory");
        assert!(t
            .step(vec![dialog(1, "App", "Fatal error: out of memory")])
            .is_empty());
        // Closed, then shown again (new or reused handle): reported again.
        assert!(t.step(vec![]).is_empty());
        assert_eq!(t.step(vec![dialog(1, "App", "Fatal error")]).len(), 1);
    }

    /// Real Win32 check: needs an interactive desktop, so it is opt-in
    /// (`cargo test -p eir-ui -- --ignored real_desktop`).
    #[test]
    #[ignore = "needs an interactive desktop session"]
    fn real_desktop_error_box_and_frozen_window_are_observed() {
        let spawn = |script: &str| {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .spawn()
                .expect("start powershell")
        };
        let mut boxed = spawn(
            "Add-Type -AssemblyName System.Windows.Forms; \
             [System.Windows.Forms.MessageBox]::Show('Cannot open the database: access is denied.', 'EirWatchTest') | Out-Null",
        );
        let mut frozen = spawn(
            "Add-Type -AssemblyName System.Windows.Forms; $f = New-Object System.Windows.Forms.Form; \
             $f.Text = 'EirHangTest'; $f.Show(); [System.Threading.Thread]::Sleep(40000)",
        );
        let mut tracker = Tracker::default();
        let (mut dialog, mut hang) = (None, None);
        for _ in 0..20 {
            std::thread::sleep(POLL);
            for r in tracker.step(observe(std::process::id())) {
                if r.title == "EirWatchTest" {
                    dialog = Some(r);
                } else if r.title == "EirHangTest" {
                    hang = Some(r);
                }
            }
            if dialog.is_some() && hang.is_some() {
                break;
            }
        }
        for child in [&mut boxed, &mut frozen] {
            let _ = child.kill();
            let _ = child.wait();
        }
        let dialog = dialog.expect("the error message box was observed");
        assert_eq!(dialog.app.to_lowercase(), "powershell.exe");
        assert!(dialog.text.contains("access is denied"), "{dialog:?}");
        assert!(!dialog.hung);
        let hang = hang.expect("the frozen window was reported as hung");
        assert!(hang.hung);
        assert_eq!(hang.app.to_lowercase(), "powershell.exe");
    }

    #[test]
    fn a_hang_is_reported_only_after_two_consecutive_polls_and_once() {
        let mut t = Tracker::default();
        assert!(
            t.step(vec![hung(7)]).is_empty(),
            "a single slow poll is not a hang"
        );
        let r = t.step(vec![hung(7)]);
        assert_eq!(r.len(), 1);
        assert!(r[0].hung);
        assert_eq!(r[0].app, "word.exe");
        assert!(t.step(vec![hung(7)]).is_empty(), "still the same hang");
        // It recovered; a later hang is a new event.
        assert!(t.step(vec![]).is_empty());
        assert!(t.step(vec![hung(7)]).is_empty());
        assert_eq!(t.step(vec![hung(7)]).len(), 1);
    }
}
