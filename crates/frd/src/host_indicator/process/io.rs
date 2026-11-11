//! One native-I/O thread, private socketpairs and the read-only UI protocol.
#[cfg(test)]
use super::TURN;
use super::{CHECK_BUDGET, CHECK_INTERVAL, Error, ProcessLaunch, Role, Shared, deadline, now_ns};
use crate::input_process::kill_group;
use fr_core::{
    indicator_process::{FRAME_BYTES, Frame, Kind},
    input_submission::process::{self, Signal},
};
use rustix::event::{PollFd, PollFlags};
#[cfg(test)]
use std::thread;
use std::{
    io::{self, Read, Write},
    os::unix::net::{UnixDatagram, UnixStream},
    process::Child,
    time::Duration,
};

pub(super) fn run(launch: &ProcessLaunch, shared: &Shared, role: Role) -> Result<(), Error> {
    if let Err(error) = shared.check() {
        shared.stop(error);
        return Ok(());
    }
    let (child, command, signals) =
        match crate::session_ui_process::spawn(launch).map_err(|_| Error::Unavailable) {
            Ok(created) => created,
            Err(error) => {
                shared.stop(error);
                return Ok(());
            }
        };
    let mut process = Process {
        child: Some(child),
        command,
        signals,
        shared,
    };
    let error = process
        .serve(launch.epoch, role)
        .err()
        .unwrap_or(Error::Stopped);
    shared.stop(error);
    process.finish()
}
struct Process<'a> {
    child: Option<Child>,
    command: UnixStream,
    signals: UnixDatagram,
    shared: &'a Shared,
}
impl Process<'_> {
    fn serve(&mut self, epoch: u128, role: Role) -> Result<(), Error> {
        let until = self.shared.state().until;
        self.exchange(
            Frame {
                kind: match role {
                    Role::Observation => Kind::Open,
                    Role::Microphone => Kind::OpenMicrophone,
                },
                sequence: 1,
                epoch,
            },
            until,
        )?;
        let mut sequence = 2_u64;
        loop {
            // An immediate Check after Open establishes the first short lease.
            // Mapping latency never becomes a three-second liveness extension.
            let issued = now_ns()?;
            let until = deadline(issued, CHECK_BUDGET)?;
            self.exchange(
                Frame {
                    kind: match role {
                        Role::Observation => Kind::Check,
                        Role::Microphone => Kind::CheckMicrophone,
                    },
                    sequence,
                    epoch,
                },
                until,
            )?;
            self.shared.ready(issued)?;
            sequence = sequence.checked_add(1).ok_or(Error::Protocol)?;
            let next = deadline(issued, CHECK_INTERVAL)?;
            while now_ns()? < next {
                self.check()?;
                let mut byte = [0; 1];
                match self.command.read(&mut byte) {
                    Ok(0) => return Err(self.failure(Error::Unavailable)),
                    Ok(_) => return Err(Error::Protocol),
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(self.failure(Error::Unavailable)),
                }
                self.wait(false, next)?;
            }
        }
    }
    fn check(&self) -> Result<(), Error> {
        self.shared.check()?;
        if self.child.is_none() {
            return Err(Error::Stopped);
        }
        // Do not call try_wait here: reaping would allow PID/process-group ID
        // reuse before finish sends its signal. EOF and bounded missing replies
        // detect death while the original child remains unreaped and owned.
        signal(&self.signals)
    }
    fn failure(&self, fallback: Error) -> Error {
        // The read-only child sends its terminal signal BEFORE closing the
        // command socket or exiting. Those are different queues: an earlier
        // check may have seen no datagram just before a command read sees EOF.
        // Drain that terminal record before classifying generic child loss.
        // Existing terminal evidence still wins; this never revives the owner.
        self.check().err().unwrap_or(fallback)
    }
    fn wait(&self, writing: bool, until: u64) -> Result<(), Error> {
        self.check()?;
        let until = until.min(self.shared.state().until);
        let remaining = Duration::from_nanos(until.saturating_sub(now_ns()?));
        self.shared
            .native_wake
            .wait(
                PollFd::new(
                    &self.command,
                    if writing { PollFlags::OUT } else { PollFlags::IN },
                ),
                Some(PollFd::new(&self.signals, PollFlags::IN)),
                remaining,
            )
            .map_err(|_| self.failure(Error::Unavailable))?;
        // Terminal evidence has priority over command readiness, including
        // local revoke arriving while a response or write was in progress.
        self.check()
    }
    fn reply(&self, request: Frame, bytes: &[u8]) -> Result<(), Error> {
        match response(request, bytes) {
            Err(Error::Unavailable) => Err(self.failure(Error::Unavailable)),
            // A newly queued signal cannot relabel an already observed
            // malformed response as a graceful local stop.
            result => result,
        }
    }
    fn exchange(&mut self, request: Frame, until: u64) -> Result<(), Error> {
        let bytes = request.encode().map_err(|_| Error::Protocol)?;
        let mut output = 0;
        let mut input = [0; FRAME_BYTES];
        let mut filled = 0;
        loop {
            self.check()?;
            if now_ns()? >= until {
                return Err(Error::Expired);
            }
            if output < bytes.len() {
                match self.command.write(&bytes[output..]) {
                    Ok(0) => return Err(self.failure(Error::Unavailable)),
                    Ok(n) => output += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(self.failure(Error::Unavailable)),
                }
            }
            if output == bytes.len() {
                match self.command.read(&mut input[filled..]) {
                    Ok(0) => return Err(self.failure(Error::Unavailable)),
                    Ok(n) => filled += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(self.failure(Error::Unavailable)),
                }
                if filled == input.len() {
                    self.check()?;
                    if now_ns()? >= until {
                        return Err(Error::Expired);
                    }
                    return self.reply(request, &input);
                }
            }
            self.wait(output < bytes.len(), until)?;
        }
    }
    fn finish(&mut self) -> Result<(), Error> {
        self.shared.stop(Error::Stopped);
        let Some(child) = &mut self.child else {
            return Ok(());
        };
        kill_group(child);
        // A read-only UI holds no remote input state. Killing and reaping the
        // original process closes its UI resources even when native UI is stuck.
        // A stuck wait retains the global worker permit instead of replacing it.
        child.wait().map_err(|_| Error::Cleanup)?;
        self.child = None;
        Ok(())
    }
}
impl Drop for Process<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
fn signal(signals: &UnixDatagram) -> Result<(), Error> {
    // Every accepted datagram is terminal, so consume at most one. The extra
    // byte detects truncation instead of accepting an oversized valid prefix.
    // EINTR is retried, not mistaken for an empty queue; the attempt bound also
    // prevents a stream of OS signals from monopolizing this native-I/O turn.
    let mut bytes = [0; process::SIGNAL_BYTES + 1];
    for _ in 0..4 {
        match signals.recv(&mut bytes) {
            Ok(n) => {
                return match process::decode_signal(&bytes[..n]) {
                    Ok(Signal::LocalRevoke) => Err(Error::LocalRevoke),
                    Ok(Signal::IndicatorFailed) => Err(Error::Unavailable),
                    // Fence travels in the other direction. A partial or
                    // malformed packet is never combined with a later one.
                    Ok(Signal::Fence) | Err(_) => Err(Error::Protocol),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(_) => return Err(Error::Unavailable),
        }
    }
    Err(Error::Unavailable)
}
fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
pub(super) fn response(request: Frame, bytes: &[u8]) -> Result<(), Error> {
    match Frame::decode(bytes).and_then(|reply| reply.response_to(request)) {
        Ok(Kind::Ready) => Ok(()),
        Ok(Kind::Refused) => Err(Error::Unavailable),
        _ => Err(Error::Protocol),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OPEN_BUDGET, State, Status};
    use super::*;
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    fn queued_terminal(tag: &str, classify: impl FnOnce(&Process<'_>) -> Error) -> Error {
        const FIXTURE: &str = r"#!/usr/bin/python3
import socket
command = socket.socket(fileno=0)
signal = socket.socket(fileno=1)
assert command.recv(1) == b'x'
signal.send(bytes([@SIGNAL@]))
raise SystemExit(0)
";
        let path = std::env::temp_dir().join(format!(
            "fr-indicator-eof-order-{}-{tag}.py",
            std::process::id()
        ));
        let bytes = process::encode_signal(Signal::LocalRevoke)
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        crate::executable::write(&path, FIXTURE.replace("@SIGNAL@", &bytes));
        let display = if cfg!(target_os = "macos") { "" } else { ":0" };
        let launch = ProcessLaunch::new(&path, display, None, 37).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let original = stopped.clone();
        let now = now_ns().unwrap();
        let shared = Shared {
            on_stop: Box::new(move || original.store(true, Ordering::Release)),
            state: Mutex::new(State {
                status: Status::Opening,
                until: deadline(now, OPEN_BUDGET).unwrap(),
                last: now,
            }),
            native_wake: crate::native_wait::Wake::new().unwrap(),
        };
        let (child, command, signals) = crate::session_ui_process::spawn(&launch).unwrap();
        let mut owner = Process {
            child: Some(child),
            command,
            signals,
            shared: &shared,
        };
        // Make the problematic order deterministic: no signal was present at
        // the normal pre-check, then the child queues revoke and closes before
        // the command reader consumes its EOF. The final drain must find it.
        assert_eq!(owner.check(), Ok(()));
        owner.command.write_all(b"x").unwrap();
        let until = Instant::now() + OPEN_BUDGET;
        loop {
            match owner.command.read(&mut [0]) {
                Ok(0) => break,
                Err(error) if retry(&error) => {}
                result => panic!("expected command EOF: {result:?}"),
            }
            assert!(Instant::now() < until);
            thread::sleep(TURN);
        }
        let cause = classify(&owner);
        shared.stop(cause);
        assert!(stopped.load(Ordering::Acquire));
        assert_eq!(owner.failure(Error::Unavailable), cause);
        assert_eq!(owner.finish(), Ok(()));
        assert!(owner.child.is_none(), "exact original child was collected");
        assert_eq!(shared.status(), Status::Stopped(cause));
        cause
    }

    #[test]
    fn a_signal_queued_after_the_precheck_wins_over_observed_command_eof() {
        assert_eq!(
            queued_terminal("eof", |owner| owner.failure(Error::Unavailable)),
            Error::LocalRevoke
        );
    }

    #[test]
    fn a_malformed_reply_cannot_be_relabelled_by_a_queued_revoke() {
        assert_eq!(
            queued_terminal("malformed", |owner| {
                let request = Frame {
                    kind: Kind::Check,
                    sequence: 2,
                    epoch: 37,
                };
                owner.reply(request, &[0; FRAME_BYTES]).unwrap_err()
            }),
            Error::Protocol
        );
    }
}
