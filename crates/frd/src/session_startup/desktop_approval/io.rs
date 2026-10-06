//! No native call or process wait escapes this one retained I/O worker.
use super::{Error, OPEN_BUDGET, POLL_INTERVAL, REPLY_BUDGET, Report, Shared, TURN};
use crate::{input_process::{ProcessLaunch, kill_group}, session_ui_process};
use fr_core::indicator_process::{FRAME_BYTES, Frame, Kind};
use fr_wire::negotiation::Role;
use std::{
    io::{self, Read, Write},
    os::unix::net::{UnixDatagram, UnixStream},
    process::Child,
    thread,
    time::Instant,
};

pub(super) fn run(launch: &ProcessLaunch, shared: &Shared) -> Report {
    let opened = shared.check().and_then(|()| {
        session_ui_process::spawn(launch).map_err(|_| Error::Unavailable)
    });
    let (child, command, signals) = match opened {
        Ok(opened) => opened,
        Err(error) => return Report { decision: Err(error), cleanup: Ok(()) },
    };
    let mut process = Process { child: Some(child), command, signals, shared };
    let decision = process.serve(launch.epoch);
    // A terminal native reply is not enough: actual process retirement precedes
    // caller-side consumption. Native I/O failure is not hidden by cleanup.
    let cleanup = process.finish();
    Report { decision, cleanup }
}
struct Process<'a> {
    child: Option<Child>,
    command: UnixStream,
    signals: UnixDatagram,
    shared: &'a Shared,
}
impl Process<'_> {
    fn check(&self) -> Result<(), Error> {
        self.shared.check()?;
        let mut bytes = [0; 17];
        match self.signals.recv(&mut bytes) {
            Ok(_) => Err(Error::Cancelled),
            Err(error) if retry(&error) => Ok(()),
            Err(_) => Err(Error::Unavailable),
        }
    }
    fn serve(&mut self, epoch: u128) -> Result<bool, Error> {
        let kind = match self.shared.original.role() {
            Role::Observe => Kind::ApproveView,
            Role::RequestControl => Kind::ApproveControl,
        };
        let mut request = Frame { kind, sequence: 1, epoch };
        let mut until = self.shared.started.checked_add(OPEN_BUDGET).ok_or(Error::Expired)?;
        loop {
            match self.exchange(request, until)? {
                Some(decision) => return Ok(decision),
                None => {}
            }
            let next = Instant::now().checked_add(POLL_INTERVAL).ok_or(Error::Expired)?;
            while Instant::now() < next {
                self.check()?;
                let mut byte = [0; 1];
                match self.command.read(&mut byte) {
                    Err(error) if retry(&error) => {}
                    _ => return Err(Error::Protocol),
                }
                thread::sleep(TURN);
            }
            request = Frame {
                kind: Kind::ApprovalCheck,
                sequence: request.sequence.checked_add(1).ok_or(Error::Protocol)?,
                epoch,
            };
            until = Instant::now().checked_add(REPLY_BUDGET).ok_or(Error::Expired)?;
        }
    }
    fn exchange(&mut self, request: Frame, until: Instant) -> Result<Option<bool>, Error> {
        let output = request.encode().map_err(|_| Error::Protocol)?;
        let (mut written, mut filled) = (0, 0);
        let mut input = [0; FRAME_BYTES];
        loop {
            self.check()?;
            if Instant::now() >= until { return Err(Error::Expired); }
            if written < output.len() {
                match self.command.write(&output[written..]) {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => written += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(Error::Unavailable),
                }
            }
            if written == output.len() {
                match self.command.read(&mut input[filled..]) {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => filled += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(Error::Unavailable),
                }
                if filled == input.len() {
                    self.check()?;
                    if Instant::now() >= until { return Err(Error::Expired); }
                    return response(request, &input);
                }
            }
            thread::sleep(TURN);
        }
    }
    fn finish(&mut self) -> Result<(), Error> {
        let Some(child) = self.child.as_mut() else { return Ok(()); };
        // Never reap during polling: the unreaped child reserves the original
        // process-group ID until this signal. No replacement or unrelated PID.
        kill_group(child);
        child.wait().map_err(|_| Error::Cleanup)?;
        self.child = None;
        Ok(())
    }
}
impl Drop for Process<'_> {
    fn drop(&mut self) { let _ = self.finish(); }
}
fn retry(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}
pub(super) fn response(request: Frame, bytes: &[u8]) -> Result<Option<bool>, Error> {
    // This adapter cannot be used to reinterpret ordinary indicator readiness
    // or attach an answer from the input executor's protocol.
    if !matches!(request.kind, Kind::ApproveView | Kind::ApproveControl | Kind::ApprovalCheck) {
        return Err(Error::Protocol);
    }
    match Frame::decode(bytes).and_then(|reply| reply.response_to(request)) {
        Ok(Kind::Ready) => Ok(None),
        Ok(Kind::Allowed) => Ok(Some(true)),
        Ok(Kind::Denied) => Ok(Some(false)),
        Ok(Kind::Refused) => Err(Error::Unavailable),
        _ => Err(Error::Protocol),
    }
}
