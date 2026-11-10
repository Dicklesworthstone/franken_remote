//! Read-only macOS observation role. `AppKit` stays on the main thread; one
//! separate nonblocking IPC/watchdog worker owns bounded records and checks
//! original native evidence even while opening, pumping or cleanup is stuck.
use crate::{pipe, role::Observation};
use fr_core::{
    indicator_process::{FRAME_BYTES, Frame, Kind},
    input_submission::process::{self, Signal},
};
use fr_native::{
    clock,
    macos_observation::{self, Control, Panels, Status, Stop},
};
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixDatagram, UnixStream},
    },
    thread,
    time::Duration,
};

const USAGE: i32 = 64;
const PROTOCOL: i32 = 65;
const REFUSED: i32 = 66;
const CHANNEL: i32 = 67;
const HELLO_WAIT: Duration = Duration::from_secs(3);
const TURN: Duration = Duration::from_millis(5);
const PUMP: Duration = Duration::from_millis(20);
const OPEN_NS: u64 = 2_000_000_000;
const CHECK_NS: u64 = 250_000_000;
const IDLE_NS: u64 = 1_000_000_000;

pub(crate) fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let parent = match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--parent-pid"), Some(pid), None) => pid.parse::<u32>().ok(),
        _ => None,
    };
    if parent.is_none_or(|pid| macos_observation::bind_parent(pid).is_err()) {
        return USAGE;
    }
    let Ok((mut command, signals)) = channels() else {
        return CHANNEL;
    };
    let hello = match pipe::read(&mut command, HELLO_WAIT) {
        Ok(frame) => frame,
        Err(code) => return code,
    };
    let Some(role) = Observation::open(hello) else {
        // No native owner exists for an approval/microphone or foreign role.
        if hello.kind.is_request() {
            let _ = pipe::send(&mut command, hello, Kind::Refused);
        }
        return PROTOCOL;
    };
    if command.set_nonblocking(true).is_err() {
        return CHANNEL;
    }
    let control = Control::opening();
    let responder = control.clone();
    let worker = thread::Builder::new()
        .name("fr-observation-ipc".into())
        .spawn(move || {
            let result = serve(&mut command, &signals, &responder, role, hello);
            let code = finish(&signals, &responder, result);
            // This child holds no input state. Ending the exact process closes
            // its UI even if the main thread is stuck inside a native call.
            // The broker retains its child and performs the actual reap.
            std::process::exit(code);
        });
    let Ok(worker) = worker else {
        control.fail();
        return CHANNEL;
    };
    let mut panels = Panels::open(control.clone()).ok();
    while matches!(control.status(), Status::Opening | Status::Shown) {
        if let Some(panels) = panels.as_mut() {
            panels.turn(PUMP);
        } else {
            control.fail();
        }
    }
    drop(panels);
    // Native cleanup may hang; the independent worker still expires/terminates
    // this read-only process. A terminal flag never pretends cleanup finished.
    let _ = worker.join();
    CHANNEL
}

fn channels() -> io::Result<(UnixStream, UnixDatagram)> {
    let command = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
    let signals = UnixDatagram::from(io::stdout().as_fd().try_clone_to_owned()?);
    command.peer_addr()?;
    signals.peer_addr()?;
    signals.set_nonblocking(true)?;
    Ok((command, signals))
}

/// Only this worker publishes terminal datagrams, exactly once before exit.
/// In particular it cannot close the command channel before `LocalRevoke` is
/// queued, which would make an intentional local Stop look like UI failure.
fn finish(signals: &UnixDatagram, control: &Control, result: Result<(), i32>) -> i32 {
    if matches!(control.status(), Status::Opening | Status::Shown) {
        control.fail();
    }
    match control.status() {
        Status::Stopped(Stop::Local) => {
            notify(signals, Signal::LocalRevoke);
            0
        }
        Status::Stopped(Stop::AuthorityEnded) => result.err().unwrap_or(0),
        _ => {
            notify(signals, Signal::IndicatorFailed);
            result.err().unwrap_or(REFUSED)
        }
    }
}

fn notify(signals: &UnixDatagram, signal: Signal) {
    let bytes = process::encode_signal(signal);
    let first = clock::monotonic_ns();
    let until = first.and_then(|now| now.checked_add(20_000_000));
    let mut last = first;
    loop {
        match signals.send(&bytes) {
            Err(error) if retry(&error) => {}
            // Datagram writes are atomic. Never duplicate a completed send.
            Ok(_) | Err(_) => return,
        }
        let now = clock::monotonic_ns();
        if !matches!((last, now, until), (Some(last), Some(now), Some(until))
            if now >= last && now < until)
        {
            return;
        }
        last = now;
        // An interrupted syscall must not turn an intentional local Stop into
        // EOF before its signal was queued. Retry only within this original,
        // sleep-aware bound; a broken parent cannot hold UI cleanup forever.
        thread::sleep(Duration::from_millis(1));
    }
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
    AwaitVisibility {
        request: Frame,
        floor: u64,
        until: u64,
    },
    Closing {
        request: Frame,
        until: u64,
    },
    Reply {
        bytes: [u8; FRAME_BYTES],
        sent: usize,
        floor: Option<u64>,
        until: u64,
        finished: bool,
    },
}
impl Stage {
    const fn until(&self) -> u64 {
        match self {
            Self::Read { until, .. }
            | Self::AwaitVisibility { until, .. }
            | Self::Closing { until, .. }
            | Self::Reply { until, .. } => *until,
        }
    }
    const fn closing(&self) -> bool {
        matches!(
            self,
            Self::Closing { .. } | Self::Reply { finished: true, .. }
        )
    }
    fn reply(
        request: Frame,
        kind: Kind,
        floor: Option<u64>,
        until: u64,
        finished: bool,
    ) -> Result<Self, i32> {
        Ok(Self::Reply {
            bytes: request.reply(kind).encode().map_err(|_| PROTOCOL)?,
            sent: 0,
            floor,
            until,
            finished,
        })
    }
}

enum Progress {
    Waiting,
    Next(Stage),
    Finished,
}

fn serve(
    command: &mut UnixStream,
    signals: &UnixDatagram,
    control: &Control,
    mut role: Observation,
    hello: Frame,
) -> Result<(), i32> {
    let mut time = Time(0);
    let floor = time.sample()?;
    let mut stage = Stage::AwaitVisibility {
        request: hello,
        floor,
        until: floor.checked_add(OPEN_NS).ok_or(CHANNEL)?,
    };
    loop {
        if fenced(signals)? {
            control.stop();
            return Ok(());
        }
        match control.status() {
            Status::Opening | Status::Shown => {}
            Status::Stopped(Stop::AuthorityEnded) if stage.closing() => {}
            Status::Stopped(_) => return Err(REFUSED),
        }
        if time.sample()? >= stage.until() {
            return Err(REFUSED);
        }
        match advance(&mut stage, command, control, &mut role, &mut time)? {
            Progress::Waiting => {}
            Progress::Next(next) => stage = next,
            Progress::Finished => return Ok(()),
        }
        thread::sleep(TURN);
    }
}

fn fenced(signals: &UnixDatagram) -> Result<bool, i32> {
    let mut bytes = [0; process::SIGNAL_BYTES + 1];
    match signals.recv(&mut bytes) {
        Ok(count) if process::decode_signal(&bytes[..count]) == Ok(Signal::Fence) => Ok(true),
        // A malformed or wrong-direction signal terminates without claiming
        // the local user requested it. There is no resume/approval signal.
        Ok(_) => Err(PROTOCOL),
        Err(error) if retry(&error) => Ok(false),
        Err(_) => Err(CHANNEL),
    }
}

fn advance(
    stage: &mut Stage,
    command: &mut UnixStream,
    control: &Control,
    role: &mut Observation,
    time: &mut Time,
) -> Result<Progress, i32> {
    match stage {
        Stage::Read {
            bytes,
            filled,
            until,
        } => {
            match command.read(&mut bytes[*filled..]) {
                Ok(0) => return Err(CHANNEL),
                Ok(count) => *filled += count,
                Err(error) if retry(&error) => return Ok(Progress::Waiting),
                Err(_) => return Err(CHANNEL),
            }
            if time.sample()? >= *until {
                return Err(REFUSED);
            }
            if *filled != FRAME_BYTES {
                return Ok(Progress::Waiting);
            }
            let request = Frame::decode(bytes).map_err(|_| PROTOCOL)?;
            // This stamp follows receipt of the complete command. Completion
            // of an older native turn cannot satisfy this request's floor.
            let floor = time.sample()?;
            let until = floor.checked_add(CHECK_NS).ok_or(CHANNEL)?;
            let next = match role.accept(request) {
                Some(Kind::Check) => Stage::AwaitVisibility {
                    request,
                    floor,
                    until,
                },
                Some(Kind::Stop) => {
                    control.stop();
                    Stage::Closing { request, until }
                }
                _ => return Err(PROTOCOL),
            };
            Ok(Progress::Next(next))
        }
        Stage::AwaitVisibility {
            request,
            floor,
            until,
        } => {
            if control.ready_since(*floor) {
                Stage::reply(*request, Kind::Ready, Some(*floor), *until, false).map(Progress::Next)
            } else {
                Ok(Progress::Waiting)
            }
        }
        Stage::Closing { request, until } => {
            if control.is_closed() {
                Stage::reply(*request, Kind::Stopped, None, *until, true).map(Progress::Next)
            } else {
                Ok(Progress::Waiting)
            }
        }
        Stage::Reply {
            bytes,
            sent,
            floor,
            until,
            finished,
        } => {
            if floor.is_some_and(|floor| !control.ready_since(floor)) {
                return Err(REFUSED);
            }
            match command.write(&bytes[*sent..]) {
                Ok(0) => return Err(CHANNEL),
                Ok(count) => *sent += count,
                Err(error) if retry(&error) => return Ok(Progress::Waiting),
                Err(_) => return Err(CHANNEL),
            }
            let now = time.sample()?;
            if now >= *until {
                return Err(REFUSED);
            }
            if *sent != FRAME_BYTES {
                return Ok(Progress::Waiting);
            }
            if *finished {
                return Ok(Progress::Finished);
            }
            Ok(Progress::Next(Stage::Read {
                bytes: [0; FRAME_BYTES],
                filled: 0,
                until: now.checked_add(IDLE_NS).ok_or(CHANNEL)?,
            }))
        }
    }
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
