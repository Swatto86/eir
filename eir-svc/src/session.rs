use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
};

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
}
