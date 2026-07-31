use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf, Prefix};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTS_CURRENT_SERVER_HANDLE,
    WTS_SESSION_INFOW,
};
use windows::Win32::UI::Shell::GetUserProfileDirectoryW;

/// The sole active interactive user session, whether console or RDP. Multiple
/// simultaneous active users fail closed because Eir has no user-selection UI.
pub fn active_user_session_id() -> Option<u32> {
    let mut sessions = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count) }
        .ok()?;
    let active = if sessions.is_null() {
        None
    } else {
        let sessions = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
        sole_active_session(sessions)
    };
    if !sessions.is_null() {
        unsafe { WTSFreeMemory(sessions.cast()) };
    }
    active
}

pub fn active_user_profile_dir() -> Option<PathBuf> {
    let session = active_user_session_id()?;
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
    let result = user_profile_dir_for_token(token);
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

pub(crate) fn user_profile_dir_for_token(token: HANDLE) -> Option<PathBuf> {
    let mut chars = 0;
    unsafe {
        let _ = GetUserProfileDirectoryW(token, PWSTR::null(), &mut chars);
    }
    if chars == 0 || chars > 32_768 {
        return None;
    }
    let mut buffer = vec![0u16; chars as usize];
    unsafe {
        GetUserProfileDirectoryW(token, PWSTR(buffer.as_mut_ptr()), &mut chars).ok()?;
    }
    let end = buffer.iter().position(|&c| c == 0)?;
    Some(PathBuf::from(OsString::from_wide(&buffer[..end])))
}

pub fn system_drive_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .and_then(|windows| drive_root(Path::new(&windows)))
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn drive_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let prefix = match components.next()? {
        Component::Prefix(prefix)
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) =>
        {
            prefix
        }
        _ => return None,
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return None;
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push("\\");
    Some(root)
}

fn sole_active_session(sessions: &[WTS_SESSION_INFOW]) -> Option<u32> {
    let mut active = sessions
        .iter()
        .filter(|session| {
            session.State == WTSActive && session.SessionId != 0 && session.SessionId != u32::MAX
        })
        .map(|session| session.SessionId);
    let selected = active.next()?;
    active.next().is_none().then_some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::RemoteDesktop::WTSDisconnected;

    #[test]
    fn selects_one_active_console_or_rdp_session_and_fails_closed_on_multiple() {
        let active = WTS_SESSION_INFOW {
            SessionId: 2,
            State: WTSActive,
            ..Default::default()
        };
        let disconnected = WTS_SESSION_INFOW {
            SessionId: 3,
            State: WTSDisconnected,
            ..Default::default()
        };
        assert_eq!(sole_active_session(&[active, disconnected]), Some(2));
        assert_eq!(sole_active_session(&[active, active]), None);
    }

    #[test]
    fn derives_the_real_windows_drive_root() {
        assert_eq!(
            drive_root(Path::new("D:\\Windows")),
            Some(PathBuf::from("D:\\"))
        );
        assert_eq!(
            drive_root(Path::new("\\\\?\\E:\\Windows")),
            Some(PathBuf::from("\\\\?\\E:\\"))
        );
        assert_eq!(drive_root(Path::new("\\\\server\\share\\Windows")), None);
        assert_eq!(drive_root(Path::new("Windows")), None);
    }
}
