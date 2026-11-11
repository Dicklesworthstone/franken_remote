//! Bounded readiness on a supervised child's existing descriptors. This is used
//! only on native-I/O threads, never on the authority/runtime thread. A permanent
//! local shutdown wakes an outstanding poll without periodic sleeps or a queue.
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{io, net::Shutdown, os::unix::net::UnixStream, time::Duration};

pub(crate) struct Wake {
    read: UnixStream,
    stop: UnixStream,
}
impl Wake {
    pub(crate) fn new() -> io::Result<Self> {
        let (read, stop) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        stop.set_nonblocking(true)?;
        Ok(Self { read, stop })
    }

    /// Terminal and idempotent. EOF is level-triggered and cannot overflow a
    /// wake-byte buffer or get consumed before a later wait. Shared status, not
    /// this descriptor, still owns the stop reason and all authority decisions.
    pub(crate) fn signal(&self) {
        for _ in 0..4 {
            match self.stop.shutdown(Shutdown::Write) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                _ => break,
            }
        }
    }

    /// Wait for one command pipe and, optionally, one terminal signal socket.
    /// Callers compute `remaining` from their immutable absolute deadline and
    /// recheck their original status/clock after EVERY return, including EINTR.
    /// No descriptor is read, written, drained, replaced or acknowledged here.
    pub(crate) fn wait(
        &self,
        command: PollFd<'_>,
        signal: Option<PollFd<'_>>,
        remaining: Duration,
    ) -> io::Result<()> {
        let mut fds = [
            PollFd::new(&self.read, PollFlags::IN),
            command,
            signal.unwrap_or_else(|| PollFd::new(&self.read, PollFlags::IN)),
        ];
        let timeout = Timespec::try_from(remaining)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests;
