//! Portable policy for the Windows private process boundary.
#![forbid(unsafe_code)]
use super::{Duration, Instant, Shared};
use std::io::{self, ErrorKind};

/// Only an ordinary absolute drive path (optionally verbatim) is admitted.
/// UNC/device namespaces, alternate streams and Win32 name aliases must never
/// change the executable or directory whose handles we inspect and pin.
pub(super) fn image_root(path: &str) -> io::Result<usize> {
    let offset = if path.starts_with(r"\\?\") { 4 } else { 0 };
    let drive = &path.as_bytes()[offset..];
    if path.len() > 128_000
        || drive.len() < 4
        || !drive[0].is_ascii_alphabetic()
        || drive[1] != b':'
        || drive[2] != b'\\'
        || path.encode_utf16().count() > 32_000
    {
        return Err(ErrorKind::InvalidInput.into());
    }
    let root = offset + 3;
    let mut count = 0usize;
    for component in path[root..].split('\\') {
        count += 1;
        if count > 64
            || component.is_empty()
            || component.ends_with(['.', ' '])
            || component.starts_with(' ')
            || component.encode_utf16().count() > 255
            || component.chars().any(|c| {
                c.is_control() || matches!(c, '/' | ':' | '"' | '<' | '>' | '|' | '?' | '*')
            })
        {
            return Err(ErrorKind::InvalidInput.into());
        }
        let base = component.split('.').next().unwrap_or_default();
        if matches!(
            base.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL"
        ) || (base.len() == 4
            && base.get(..3).is_some_and(|prefix| {
                prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT")
            })
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(ErrorKind::InvalidInput.into());
        }
    }
    Ok(root)
}

pub(super) fn read_count(requested: usize, reported: usize) -> io::Result<usize> {
    if reported > requested {
        Err(ErrorKind::InvalidData.into())
    } else if requested != 0 && reported == 0 {
        Err(ErrorKind::UnexpectedEof.into())
    } else {
        Ok(reported)
    }
}

pub(super) fn write_count(requested: usize, reported: usize) -> io::Result<usize> {
    if reported > requested {
        Err(ErrorKind::InvalidData.into())
    } else if requested != 0 && reported == 0 {
        // Byte-mode PIPE_NOWAIT can succeed without accepting a single byte.
        Err(ErrorKind::WouldBlock.into())
    } else {
        Ok(reported)
    }
}

pub(super) struct Deadline<'a> {
    shared: &'a Shared,
    until: Instant,
}
impl<'a> Deadline<'a> {
    pub(super) fn new(shared: &'a Shared, until: Instant) -> Self {
        Self { shared, until }
    }
    pub(super) fn set(&mut self, until: Instant) {
        self.until = until;
    }
    fn remaining(&self, now: Instant, stopped: bool) -> io::Result<Duration> {
        if stopped {
            return Err(ErrorKind::ConnectionAborted.into());
        }
        self.until
            .checked_duration_since(now)
            .filter(|v| !v.is_zero())
            .map(|d| d.min(Duration::from_millis(10)))
            .ok_or_else(|| ErrorKind::TimedOut.into())
    }
    pub(super) fn live(&self) -> io::Result<Duration> {
        let stopped = self.shared.mail.lock().map_err(|_| ErrorKind::Other)?.stop;
        self.remaining(Instant::now(), stopped)
    }
    pub(super) fn wait(&self) -> io::Result<()> {
        let mail = self.shared.mail.lock().map_err(|_| ErrorKind::Other)?;
        let remaining = self.remaining(Instant::now(), mail.stop)?;
        // The mutex is released by the wait. Stop notifies this same original
        // owner's Condvar, so no native IPC polling sleep delays cancellation.
        let (mail, _) = self
            .shared
            .wake
            .wait_timeout(mail, remaining)
            .map_err(|_| ErrorKind::Other)?;
        self.remaining(Instant::now(), mail.stop).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::process::{AtomicBool, AtomicU32, Condvar, Mailbox, Mutex};

    fn shared() -> Shared {
        Shared {
            mail: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            retired: AtomicBool::new(false),
            pid: AtomicU32::new(0),
        }
    }
    #[test]
    fn image_policy_refuses_network_alias_stream_and_device_paths() {
        for path in [
            r"worker.exe",
            r"C:worker.exe",
            r"\worker.exe",
            r"\\server\share\worker.exe",
            r"\\?\UNC\server\share\worker.exe",
            r"\\.\C:\worker.exe",
            r"C:\app\..\worker.exe",
            r"C:\app\worker.exe:stream",
            r"C:\app.\worker.exe",
            r"C:\app\worker.exe ",
            r"C:\app\\worker.exe",
            r"C:\app\NUL.exe",
            r"C:\app\COM1.exe",
            "C:\\app\\wor\0ker.exe",
            r"C:\app/worker.exe",
        ] {
            assert!(image_root(path).is_err(), "{path:?}");
        }
        assert_eq!(
            image_root(r"C:\Program Files\Franken Remote\fr-opus-worker.exe").unwrap(),
            3
        );
        assert_eq!(
            image_root(r"\\?\C:\Program Files\Franken Remote\fr-opus-worker.exe").unwrap(),
            7
        );
        assert!(image_root(r"C:\目录\fr-opus-worker.exe").is_ok());
        assert!(image_root(r"C:\éé\fr-opus-worker.exe").is_ok());
        assert!(image_root(&format!(r"C:\{}\worker.exe", "a".repeat(256))).is_err());
        assert!(image_root(&format!(r"C:\{}worker.exe", "a\\".repeat(65))).is_err());
    }
    #[test]
    fn partial_pipe_write_keeps_its_prefix_and_zero_is_backpressure() {
        assert_eq!(write_count(100, 37).unwrap(), 37);
        assert_eq!(
            write_count(63, 0).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        assert_eq!(write_count(63, 63).unwrap(), 63);
        assert_eq!(
            write_count(63, 64).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(write_count(0, 0).unwrap(), 0);
    }
    #[test]
    fn successful_empty_read_is_eof_not_an_idle_pipe() {
        assert_eq!(
            read_count(48, 0).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(read_count(48, 7).unwrap(), 7);
        assert_eq!(
            read_count(48, 49).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(read_count(0, 0).unwrap(), 0);
    }
    #[test]
    fn partial_io_never_refreshes_original_deadline_and_stop_wins() {
        let shared = shared();
        let start = Instant::now();
        let mut gate = Deadline::new(&shared, start + Duration::from_millis(100));
        assert_eq!(
            gate.remaining(start, false).unwrap(),
            Duration::from_millis(10)
        );
        assert_eq!(
            gate.remaining(start + Duration::from_millis(99), false)
                .unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            gate.remaining(start + Duration::from_millis(100), false)
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
        // Only an explicitly accepted next request supplies another deadline.
        gate.set(start + Duration::from_millis(200));
        assert_eq!(
            gate.remaining(start + Duration::from_millis(199), false)
                .unwrap(),
            Duration::from_millis(1)
        );
        shared.stop();
        assert_eq!(
            gate.live().unwrap_err().kind(),
            ErrorKind::ConnectionAborted
        );
        assert_eq!(
            gate.wait().unwrap_err().kind(),
            ErrorKind::ConnectionAborted
        );
    }
}
