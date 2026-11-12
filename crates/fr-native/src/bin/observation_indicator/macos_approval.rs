//! Independent IPC/watchdog for one main-thread `AppKit` approval prompt.
//! Native cleanup and the original broker Approval both precede any grant.
use crate::role::Approval;
use fr_core::{
    indicator_process::{FRAME_BYTES, Frame, Kind},
    input_submission::process::{self, Signal},
};
use fr_native::{
    clock,
    macos_approval::{Control, Status, Surface},
};
use std::{
    io::{self, Read, Write},
    os::unix::net::{UnixDatagram, UnixStream},
    thread,
    time::Duration,
};

const CHANNEL: i32 = 67;
const PROTOCOL: i32 = 65;
const REFUSED: i32 = 66;
const TURN: Duration = Duration::from_millis(5);
const OPEN_NS: u64 = 2_000_000_000;
const REPLY_NS: u64 = 250_000_000;
const IDLE_NS: u64 = 1_000_000_000;

pub(crate) fn run(mut command: UnixStream, signals: UnixDatagram, hello: Frame) -> i32 {
    let Some(role) = Approval::open(hello) else {
        return PROTOCOL;
    };
    if command.set_nonblocking(true).is_err() {
        return CHANNEL;
    }
    let controlling = role.requests_control();
    let control = Control::opening();
    let mut time = Time(0);
    let Some(opening_until) = time.sample().ok().and_then(|now| now.checked_add(OPEN_NS)) else {
        return CHANNEL;
    };
    let context = crate::approval_context::read(&mut command, hello, || {
        if fenced(&signals)?
            || time.sample()? >= opening_until
            || control.status() != Status::Opening
        {
            Err(REFUSED)
        } else {
            Ok(())
        }
    });
    let details = match context.and_then(|context| context.details(hello).map_err(|_| PROTOCOL)) {
        Ok(details) => details,
        Err(error) => {
            control.cancel();
            return error;
        }
    };
    let responder = control.clone();
    let worker = thread::Builder::new()
        .name("fr-approval-ipc".into())
        .spawn(move || {
            let code = serve(
                &mut command,
                &signals,
                &responder,
                role,
                hello,
                time,
                opening_until,
            )
            .err()
            .unwrap_or(0);
            responder.cancel();
            // This child owns no input or effects. Process death removes its exact
            // UI even if native destruction is stuck; the parent still must reap it.
            std::process::exit(code);
        });
    let Ok(worker) = worker else {
        control.cancel();
        return CHANNEL;
    };
    let mut surface = Surface::open(control.clone(), controlling, &details).ok();
    while matches!(control.status(), Status::Opening | Status::Shown) {
        if let Some(surface) = &mut surface {
            surface.turn(Duration::from_millis(20));
        } else {
            control.cancel();
        }
    }
    drop(surface);
    let _ = worker.join();
    CHANNEL
}

struct Time(u64);
impl Time {
    fn sample(&mut self) -> Result<u64, i32> {
        let now = clock::monotonic_ns().ok_or(CHANNEL)?;
        if now < self.0 {
            return Err(CHANNEL);
        }
        self.0 = now;
        Ok(now)
    }
}

enum Stage {
    Read {
        bytes: [u8; FRAME_BYTES],
        filled: usize,
        until: u64,
    },
    Await {
        request: Frame,
        floor: u64,
        until: u64,
        stopping: bool,
    },
    Write {
        bytes: [u8; FRAME_BYTES],
        sent: usize,
        until: u64,
        kind: Kind,
    },
}
impl Stage {
    const fn until(&self) -> u64 {
        match self {
            Self::Read { until, .. } | Self::Await { until, .. } | Self::Write { until, .. } => {
                *until
            }
        }
    }
}

fn serve(
    command: &mut UnixStream,
    signals: &UnixDatagram,
    control: &Control,
    mut role: Approval,
    hello: Frame,
    mut time: Time,
    opening_until: u64,
) -> Result<(), i32> {
    let floor = time.sample()?;
    let mut stage = Stage::Await {
        request: hello,
        floor,
        until: opening_until,
        stopping: false,
    };
    loop {
        if fenced(signals)? || time.sample()? >= stage.until() {
            return Err(REFUSED);
        }
        let status = control.status();
        if status == Status::Refused
            && !matches!(
                stage,
                Stage::Await { stopping: true, .. }
                    | Stage::Write {
                        kind: Kind::Stopped,
                        ..
                    }
            )
        {
            return Err(REFUSED);
        }
        match &mut stage {
            Stage::Read { bytes, filled, .. } => {
                match command.read(&mut bytes[*filled..]) {
                    Ok(0) => return Err(CHANNEL),
                    Ok(count) => *filled += count,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(CHANNEL),
                }
                if *filled == FRAME_BYTES {
                    let request = Frame::decode(bytes).map_err(|_| PROTOCOL)?;
                    let stopping = match role.accept(request) {
                        Some(Kind::ApprovalCheck) => false,
                        Some(Kind::Stop) => {
                            control.cancel();
                            true
                        }
                        _ => return Err(PROTOCOL),
                    };
                    let floor = time.sample()?;
                    stage = Stage::Await {
                        request,
                        floor,
                        until: floor.checked_add(REPLY_NS).ok_or(CHANNEL)?,
                        stopping,
                    };
                }
            }
            Stage::Await {
                request,
                floor,
                until,
                stopping,
            } => {
                if let Some(kind) = reply_kind(control, status, *floor, *stopping) {
                    stage = Stage::Write {
                        bytes: request.reply(kind).encode().map_err(|_| PROTOCOL)?,
                        sent: 0,
                        until: *until,
                        kind,
                    };
                }
            }
            Stage::Write {
                bytes, sent, kind, ..
            } => {
                // A tentative local yes cannot survive stalled destruction,
                // cancellation, lost evidence or the original prompt deadline.
                if *kind == Kind::Allowed
                    && (status != Status::Decided(true) || !control.is_closed())
                {
                    return Err(REFUSED);
                }
                match command.write(&bytes[*sent..]) {
                    Ok(0) => return Err(CHANNEL),
                    Ok(count) => *sent += count,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(CHANNEL),
                }
                if *sent == FRAME_BYTES {
                    if *kind != Kind::Ready {
                        return Ok(());
                    }
                    stage = Stage::Read {
                        bytes: [0; FRAME_BYTES],
                        filled: 0,
                        until: time.sample()?.checked_add(IDLE_NS).ok_or(CHANNEL)?,
                    };
                }
            }
        }
        thread::sleep(TURN);
    }
}

fn reply_kind(control: &Control, status: Status, floor: u64, stopping: bool) -> Option<Kind> {
    if stopping {
        return control.is_closed().then_some(Kind::Stopped);
    }
    match status {
        Status::Decided(allow) if control.is_closed() => {
            Some(if allow { Kind::Allowed } else { Kind::Denied })
        }
        Status::Shown if control.ready_since(floor) => Some(Kind::Ready),
        _ => None,
    }
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn fenced(signals: &UnixDatagram) -> Result<bool, i32> {
    let mut bytes = [0; process::SIGNAL_BYTES + 1];
    match signals.recv(&mut bytes) {
        Ok(count) if process::decode_signal(&bytes[..count]) == Ok(Signal::Fence) => Ok(true),
        Ok(_) => Err(PROTOCOL),
        Err(error) if retry(&error) => Ok(false),
        Err(_) => Err(CHANNEL),
    }
}
