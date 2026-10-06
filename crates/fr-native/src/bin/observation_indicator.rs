#![forbid(unsafe_code)]
//! Read-only, parent-bound "Stop sharing" surface for an observation cohort.
//! Uses the existing device-attributed X11 indicator. This process has no input
//! executor, capture, clipboard, or network-protocol path. A separate initial
//! approval record selects its one-use device-attributed consent role.
//! The inherited sockets use a distinct fixed-size protocol; input-executor
//! records are never accepted. Process exit closes all native UI resources.
#[cfg(target_os = "linux")]
mod linux {
    use fr_core::{
        indicator_process::{FRAME_BYTES, Frame, Kind},
        input_submission::process::{self, Signal},
    };
    use fr_native::{
        bind_parent,
        sharing_indicator::{self, LocalIndicator, Status},
    };
    use std::{
        io::{self, Read, Write},
        os::{fd::AsFd, unix::net::{UnixDatagram, UnixStream}},
        sync::{Arc, atomic::{AtomicBool, Ordering}},
        thread,
        time::{Duration, Instant},
    };

    const REFUSED: i32 = 66;
    const CHANNEL: i32 = 67;
    const PROTOCOL: i32 = 65;
    const OPEN_WAIT: Duration = Duration::from_secs(3);
    const IDLE_WAIT: Duration = Duration::from_secs(1);

    pub fn run() -> i32 {
        let mut args = std::env::args().skip(1);
        let parent = match (args.next().as_deref(), args.next(), args.next()) {
            (Some("--parent-pid"), Some(pid), None) => pid.parse::<u32>().ok(),
            _ => None,
        };
        if parent.is_none_or(|pid| bind_parent(pid).is_err()) {
            return 64;
        }
        let channels = (|| -> io::Result<_> {
            let command = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
            let signals = UnixDatagram::from(io::stdout().as_fd().try_clone_to_owned()?);
            command.peer_addr()?;
            signals.peer_addr()?;
            signals.set_nonblocking(true)?;
            Ok((command, signals))
        })();
        channels.map_or(CHANNEL, |(command, signals)| serve(command, signals))
    }
    pub(crate) fn read(command: &mut UnixStream, budget: Duration) -> Result<Frame, i32> {
        let until = Instant::now().checked_add(budget).ok_or(CHANNEL)?;
        let mut bytes = [0; FRAME_BYTES];
        let mut filled = 0;
        while filled < bytes.len() {
            let left = until.checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero()).ok_or(CHANNEL)?;
            command.set_read_timeout(Some(left)).map_err(|_| CHANNEL)?;
            match command.read(&mut bytes[filled..]) {
                Ok(0) => return Err(CHANNEL),
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(CHANNEL),
            }
        }
        if Instant::now() >= until {
            return Err(CHANNEL);
        }
        Frame::decode(&bytes).map_err(|_| PROTOCOL)
    }
    pub(crate) fn send(command: &mut UnixStream, request: Frame, kind: Kind) -> Result<(), i32> {
        command.set_write_timeout(Some(IDLE_WAIT)).map_err(|_| CHANNEL)?;
        command.write_all(&request.reply(kind).encode().map_err(|_| PROTOCOL)?)
            .map_err(|_| CHANNEL)
    }
    fn stopped(signals: &UnixDatagram, revoked: &AtomicBool) -> bool {
        if revoked.load(Ordering::Acquire) {
            return true;
        }
        // Every incoming datagram is terminal, including malformed/wrong-role
        // messages. No fence can be cleared by a later command or native event.
        let mut bytes = [0; process::SIGNAL_BYTES + 1];
        !matches!(signals.recv(&mut bytes), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    }
    fn mapped(indicator: &LocalIndicator, until: Instant) -> bool {
        loop {
            match indicator.control().status() {
                Status::Mapped => return indicator.control().responsive(),
                Status::Stopped(_) => return false,
                Status::Opening if Instant::now() < until => {
                    thread::sleep(Duration::from_millis(5));
                }
                Status::Opening => return false,
            }
        }
    }
    fn serve(mut command: UnixStream, signals: UnixDatagram) -> i32 {
        let hello = match read(&mut command, OPEN_WAIT) {
            Ok(frame) if matches!(frame.kind, Kind::ApproveView | Kind::ApproveControl)
                && frame.sequence == 1 => return crate::approval::serve(command, signals, frame),
            Ok(frame) if frame.kind == Kind::Open && frame.sequence == 1 => frame,
            Ok(_) => return PROTOCOL,
            Err(error) => return error,
        };
        let revoked = Arc::new(AtomicBool::new(false));
        let flag = revoked.clone();
        let Ok(notify) = signals.try_clone() else { return CHANNEL; };
        let opened = std::env::var("DISPLAY").ok().and_then(|display| {
            sharing_indicator::start_observation_with(&display, move || {
                if !flag.swap(true, Ordering::AcqRel) {
                    let _ = notify.send(&process::encode_signal(Signal::LocalRevoke));
                }
            }).ok()
        });
        let Some(mut indicator) = opened else {
            let _ = send(&mut command, hello, Kind::Refused);
            return REFUSED;
        };
        if !mapped(&indicator, Instant::now() + OPEN_WAIT) || stopped(&signals, &revoked) {
            let _ = send(&mut command, hello, Kind::Refused);
            return REFUSED;
        }
        if send(&mut command, hello, Kind::Ready).is_err() {
            return CHANNEL;
        }
        let mut expected = 2_u64;
        loop {
            let request = match read(&mut command, IDLE_WAIT) {
                Ok(request) if request.sequence == expected && request.epoch == hello.epoch => request,
                Ok(_) => return PROTOCOL,
                Err(error) => return error,
            };
            let Some(next) = expected.checked_add(1) else { return PROTOCOL; };
            expected = next;
            match request.kind {
                Kind::Stop => {
                    indicator.control().stop();
                    let until = Instant::now() + IDLE_WAIT;
                    while indicator.finish().is_none() {
                        if Instant::now() >= until { return REFUSED; }
                        thread::sleep(Duration::from_millis(5));
                    }
                    return send(&mut command, request, Kind::Stopped).map_or(CHANNEL, |()| 0);
                }
                Kind::Check => {
                    if stopped(&signals, &revoked) || !indicator.control().responsive() {
                        let _ = send(&mut command, request, Kind::Refused);
                        return REFUSED;
                    }
                    if send(&mut command, request, Kind::Ready).is_err() { return CHANNEL; }
                }
                _ => return PROTOCOL,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fragmented_read_keeps_frame_boundaries_and_rejects_bad_magic() {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            let frame = Frame { kind: Kind::Open, sequence: 1, epoch: 7 };
            let bytes = frame.encode().unwrap();
            for part in bytes.chunks(3) { writer.write_all(part).unwrap(); }
            assert_eq!(read(&mut reader, IDLE_WAIT), Ok(frame));
            writer.write_all(&[0; FRAME_BYTES]).unwrap();
            assert_eq!(read(&mut reader, IDLE_WAIT), Err(PROTOCOL));
        }
        #[test]
        fn partial_eof_never_becomes_a_request() {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&[0; 8]).unwrap();
            drop(writer);
            assert_eq!(read(&mut reader, IDLE_WAIT), Err(CHANNEL));
        }
        #[test]
        fn any_signal_fences_without_interpreting_input_or_allowing_revival() {
            let (a, b) = UnixDatagram::pair().unwrap();
            a.set_nonblocking(true).unwrap();
            let revoked = AtomicBool::new(false);
            assert!(!stopped(&a, &revoked));
            b.send(b"not-an-approval").unwrap();
            assert!(stopped(&a, &revoked));
            revoked.store(true, Ordering::Release);
            assert!(stopped(&a, &revoked));
        }
    }
}

#[cfg(target_os = "linux")]
#[path = "observation_indicator/approval.rs"]
mod approval;

fn main() {
    #[cfg(target_os = "linux")]
    std::process::exit(linux::run());
    #[cfg(not(target_os = "linux"))]
    std::process::exit(2);
}
