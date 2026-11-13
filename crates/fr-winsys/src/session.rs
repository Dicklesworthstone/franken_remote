//! The Windows logon session a process runs in, read through the Terminal
//! Services API: whether it is attached to the console, connected and
//! unlocked, and which logon it is. Read-only; nothing here changes a session.
use std::io;
use windows::Win32::System::{
    RemoteDesktop::{
        ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK,
        WTS_SESSIONSTATE_UNLOCK, WTSActive, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
    },
    Threading::GetCurrentProcessId,
};
use windows::core::PWSTR;

/// The session this process runs in.
pub fn current() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: an out-pointer to a live local.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) }?;
    Ok(session)
}

/// The session attached to the physical console, if any (none while the
/// console is between sessions).
pub fn console() -> Option<u32> {
    // SAFETY: no arguments.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != u32::MAX).then_some(session)
}

/// Whether the session's desktop is locked, as Windows 8 and later report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Locked,
    Unlocked,
    /// Windows did not say (for example, no user is logged on).
    Unknown,
}

/// Which logon a session holds: user, domain and logon time. Compared, never
/// shown: its `Debug` names no user.
#[derive(Clone, PartialEq, Eq)]
pub struct Logon {
    user: Vec<u16>,
    domain: Vec<u16>,
    time: i64,
}
impl std::fmt::Debug for Logon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logon(..)")
    }
}
impl Logon {
    /// A user is logged on (the name is not empty).
    pub fn present(&self) -> bool {
        !self.user.is_empty()
    }
}

/// One reading of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Connected and in use (`WTSActive`); a session switched away from, or
    /// disconnected, is not.
    pub active: bool,
    pub lock: Lock,
    pub logon: Logon,
}

/// A buffer the Terminal Services API allocated.
struct Free(*mut core::ffi::c_void);
impl Drop for Free {
    fn drop(&mut self) {
        // SAFETY: the buffer WTSQuerySessionInformationW returned, freed once.
        unsafe { WTSFreeMemory(self.0) };
    }
}

fn until_nul(chars: &[u16]) -> Vec<u16> {
    chars.iter().copied().take_while(|c| *c != 0).collect()
}

/// Reads `session` (`WTSSessionInfoEx`, level 1).
pub fn read(session: u32) -> io::Result<Reading> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: out-pointers to live locals; a returned buffer is freed below
    // with `WTSFreeMemory`, once, on every path.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSSessionInfoEx,
            &raw mut buffer,
            &raw mut bytes,
        )
    }?;
    let raw = buffer.0.cast::<core::ffi::c_void>();
    let _free = Free(raw);
    if raw.is_null() || usize::try_from(bytes).map_or(true, |n| n < size_of::<WTSINFOEXW>()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: the buffer holds at least one WTSINFOEXW (checked above) and
    // lives until `_free` drops; read unaligned, so its alignment is not
    // assumed.
    let info = unsafe { raw.cast::<WTSINFOEXW>().read_unaligned() };
    if info.Level != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: level 1 selects this union member.
    let level = unsafe { info.Data.WTSInfoExLevel1 };
    if level.SessionId != session {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let lock = match u32::from_ne_bytes(level.SessionFlags.to_ne_bytes()) {
        WTS_SESSIONSTATE_LOCK => Lock::Locked,
        WTS_SESSIONSTATE_UNLOCK => Lock::Unlocked,
        _ => Lock::Unknown,
    };
    Ok(Reading {
        active: level.SessionState == WTSActive,
        lock,
        logon: Logon {
            user: until_nul(&level.UserName),
            domain: until_nul(&level.DomainName),
            time: level.LogonTime,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session this test runs in reads back with its own id, and a
    /// session id no session has refuses (the planted negative).
    #[test]
    fn this_session_reads_and_a_missing_one_refuses() {
        let session = current().unwrap();
        let reading = read(session).unwrap();
        // Twice: the logon is the same logon.
        assert_eq!(read(session).unwrap().logon, reading.logon);
        assert!(read(0x7fff_fff0).is_err());
        assert_eq!(format!("{:?}", reading.logon), "Logon(..)");
    }

    /// Needs a user logged on at the console with the desktop unlocked; run
    /// with `--ignored` on such a machine.
    #[test]
    #[ignore = "needs an unlocked console logon"]
    fn an_unlocked_console_logon_reads_active_and_unlocked() {
        let console = console().expect("a console session");
        let reading = read(console).unwrap();
        assert!(reading.active);
        assert_eq!(reading.lock, Lock::Unlocked);
        assert!(reading.logon.present());
    }
}
