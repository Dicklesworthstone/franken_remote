//! Drain one original command to a bounded terminal capture, then reap it.
//! Stderr stays inherited, so it cannot fill a second pipe behind stdout.
use fr_client::completion::{CompletionCapture, CompletionLimit, capture_completion_with_limit};
use std::{
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);

#[cfg(any(target_os = "macos", test))]
#[path = "fifo.rs"]
mod fifo;
#[cfg(target_os = "macos")]
pub(super) use fifo::run_bundle;

pub(super) struct Finished {
    pub stdout: CompletionCapture,
    /// For Launch Services this is only `open -W`'s result. A native terminal
    /// document is always required; this status never proves viewer success.
    pub status: ExitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Start,
    Capture,
    Wait,
    Cleanup,
}

pub(super) fn run(command: &mut Command) -> Result<Finished, Error> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| Error::Start)?;
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child)?;
        return Err(Error::Capture);
    };
    let nonblocking = rustix::fs::fcntl_getfl(&stdout)
        .and_then(|flags| rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK));
    if nonblocking.is_err() {
        terminate(&mut child)?;
        return Err(Error::Capture);
    }
    follow(child, stdout, None, DRAIN_TIMEOUT)
}

fn follow(
    mut child: Child,
    reader: impl Read,
    start_timeout: Option<Duration>,
    drain_timeout: Duration,
) -> Result<Finished, Error> {
    let stdout = capture_completion_with_limit(
        ChildReader {
            reader,
            child: &mut child,
            started: Instant::now(),
            saw_writer: start_timeout.is_none(),
            ending: None,
            start_timeout,
            drain_timeout,
        },
        CompletionLimit::NativeCommand,
    );
    if stdout.report.read_error.is_some() {
        // Stop and reap the original command. For Launch Services this owns
        // only the open waiter; viewer cleanup remains unconfirmed, and the
        // returned error prevents fr app from opening another session over it.
        terminate(&mut child)?;
        return Err(Error::Capture);
    }
    let Ok(status) = child.wait() else {
        terminate(&mut child)?;
        return Err(Error::Wait);
    };
    Ok(Finished { stdout, status })
}

/// The caller sets its pipe nonblocking before handing it to this owner.
/// Monitor the original process even during a flood; an inherited writer may
/// keep its pipe open after exit, but cannot keep fr app waiting indefinitely.
struct ChildReader<'a, R> {
    reader: R,
    child: &'a mut Child,
    started: Instant,
    saw_writer: bool,
    ending: Option<Instant>,
    start_timeout: Option<Duration>,
    drain_timeout: Duration,
}

impl<R: Read> Read for ChildReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            let exited = self.child.try_wait()?.is_some();
            let now = Instant::now();
            if exited {
                let ending = self.ending.get_or_insert(now);
                if now.duration_since(*ending) >= self.drain_timeout {
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
            let writer_open = match self.reader.read(out) {
                Ok(0) => false,
                Ok(count) => {
                    self.saw_writer = true;
                    if !exited {
                        self.ending = None;
                    }
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            self.saw_writer |= writer_open;
            if exited && !writer_open {
                return Ok(0);
            }
            if !self.saw_writer
                && self
                    .start_timeout
                    .is_some_and(|limit| now.duration_since(self.started) >= limit)
            {
                return Err(io::ErrorKind::TimedOut.into());
            }
            // Once either end has stopped, bound the remaining cleanup/drain.
            // An active viewer with an open, quiet stdout has no time limit.
            if exited || (self.saw_writer && !writer_open) {
                let ending = self.ending.get_or_insert(now);
                if now.duration_since(*ending) >= self.drain_timeout {
                    return Err(io::ErrorKind::TimedOut.into());
                }
            } else {
                self.ending = None;
            }
            thread::sleep(POLL);
        }
    }
}

fn terminate(child: &mut Child) -> Result<(), Error> {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    // A racing normal exit can make kill fail; a successful wait still proves
    // this original process was reaped. A failed wait remains explicit.
    let _ = child.kill();
    child.wait().map(|_| ()).map_err(|_| Error::Cleanup)
}

#[cfg(test)]
#[path = "child_tests.rs"]
mod tests;
