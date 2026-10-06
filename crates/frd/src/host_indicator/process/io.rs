//! One native-I/O thread, private socketpairs and the read-only UI protocol.
use super::{CHECK_BUDGET, CHECK_INTERVAL, Error, ProcessLaunch, Shared, TURN};
use crate::input_process::kill_group;
use fr_core::{
    indicator_process::{FRAME_BYTES, Frame, Kind},
    input_submission::process::{self, Signal},
};
use std::{
    io::{self, Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::{UnixDatagram, UnixStream}, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    thread,
    time::Instant,
};

pub(super) fn run(launch: &ProcessLaunch, shared: &Shared) -> Result<(), Error> {
    if let Err(error) = shared.check() {
        shared.stop(error);
        return Ok(());
    }
    let (child, command, signals) = match spawn(launch) {
        Ok(created) => created,
        Err(error) => {
            shared.stop(error);
            return Ok(());
        }
    };
    let mut process = Process { child: Some(child), command, signals, shared };
    let error = process.serve(launch.epoch).err().unwrap_or(Error::Stopped);
    shared.stop(error);
    process.finish()
}
/// Same validated local package/display context as the input process family,
/// but a distinct image and protocol with no input executor operations.
fn spawn(launch: &ProcessLaunch) -> Result<(Child, UnixStream, UnixDatagram), Error> {
    let (command, child_command) = UnixStream::pair().map_err(|_| Error::Unavailable)?;
    let (signals, child_signals) = UnixDatagram::pair().map_err(|_| Error::Unavailable)?;
    command.set_nonblocking(true).map_err(|_| Error::Unavailable)?;
    signals.set_nonblocking(true).map_err(|_| Error::Unavailable)?;
    let mut command_builder = Command::new(&launch.image);
    command_builder.env_clear().env("DISPLAY", &launch.display)
        .arg("--parent-pid").arg(std::process::id().to_string())
        .stdin(Stdio::from(OwnedFd::from(child_command)))
        .stdout(Stdio::from(OwnedFd::from(child_signals)))
        .stderr(Stdio::null()).process_group(0);
    if let Some(path) = &launch.xauthority {
        command_builder.env("XAUTHORITY", path);
    }
    let child = command_builder.spawn().map_err(|_| Error::Unavailable)?;
    drop(command_builder);
    Ok((child, command, signals))
}
struct Process<'a> {
    child: Option<Child>,
    command: UnixStream,
    signals: UnixDatagram,
    shared: &'a Shared,
}
impl Process<'_> {
    fn serve(&mut self, epoch: u128) -> Result<(), Error> {
        let until = self.shared.state.lock().map_err(|_| Error::Panicked)?.until;
        self.exchange(Frame { kind: Kind::Open, sequence: 1, epoch }, until)?;
        let mut sequence = 2_u64;
        loop {
            // An immediate Check after Open establishes the first short lease.
            // Mapping latency never becomes a three-second liveness extension.
            let issued = Instant::now();
            let until = issued.checked_add(CHECK_BUDGET).ok_or(Error::Expired)?;
            self.exchange(Frame { kind: Kind::Check, sequence, epoch }, until)?;
            self.shared.ready(issued)?;
            sequence = sequence.checked_add(1).ok_or(Error::Protocol)?;
            let next = issued.checked_add(CHECK_INTERVAL).ok_or(Error::Expired)?;
            while Instant::now() < next {
                self.check()?;
                let mut byte = [0; 1];
                match self.command.read(&mut byte) {
                    Err(error) if retry(&error) => {}
                    _ => return Err(Error::Protocol),
                }
                thread::sleep(TURN);
            }
        }
    }
    fn check(&mut self) -> Result<(), Error> {
        self.shared.check()?;
        if self.child.as_mut().ok_or(Error::Stopped)?.try_wait()
            .map_err(|_| Error::Unavailable)?.is_some()
        {
            return Err(Error::Unavailable);
        }
        let mut bytes = [0; process::SIGNAL_BYTES + 1];
        match self.signals.recv(&mut bytes) {
            Ok(n) if n == process::SIGNAL_BYTES
                && bytes[..n] == process::encode_signal(Signal::LocalRevoke) => Err(Error::LocalRevoke),
            Ok(_) => Err(Error::Protocol),
            Err(error) if retry(&error) => Ok(()),
            Err(_) => Err(Error::Unavailable),
        }
    }
    fn exchange(&mut self, request: Frame, until: Instant) -> Result<(), Error> {
        let bytes = request.encode().map_err(|_| Error::Protocol)?;
        let mut output = 0;
        let mut input = [0; FRAME_BYTES];
        let mut filled = 0;
        loop {
            self.check()?;
            if Instant::now() >= until { return Err(Error::Expired); }
            if output < bytes.len() {
                match self.command.write(&bytes[output..]) {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => output += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(Error::Unavailable),
                }
            }
            if output == bytes.len() {
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
        self.shared.stop(Error::Stopped);
        let Some(child) = &mut self.child else { return Ok(()); };
        kill_group(child);
        // A read-only UI holds no remote input state. Killing and reaping the
        // original process closes its X resources even when native UI is stuck.
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
fn retry(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}
pub(super) fn response(request: Frame, bytes: &[u8]) -> Result<(), Error> {
    match Frame::decode(bytes).and_then(|reply| reply.response_to(request)) {
        Ok(Kind::Ready) => Ok(()),
        Ok(Kind::Refused) => Err(Error::Unavailable),
        _ => Err(Error::Protocol),
    }
}
