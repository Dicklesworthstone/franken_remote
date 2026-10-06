//! A distinct, one-request child role. The parent keeps the actual pending
//! admission capability; these messages only report the native UI's decision.
use super::linux::{read, send};
use fr_core::indicator_process::{Frame, Kind};
use fr_native::approval_surface::{Role, State, Surface};
use std::{
    io,
    os::unix::net::{UnixDatagram, UnixStream},
    thread,
    time::{Duration, Instant},
};

const CHANNEL: i32 = 67;
const PROTOCOL: i32 = 65;
const REFUSED: i32 = 66;
const IDLE_WAIT: Duration = Duration::from_secs(1);
const PROMPT_WAIT: Duration = Duration::from_secs(30);

fn fenced(signals: &UnixDatagram) -> bool {
    let mut bytes = [0; 17];
    // Any signal is terminal. A signal channel cannot approve or resume.
    !matches!(signals.recv(&mut bytes), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
}
fn intent(frame: Frame) -> Result<Role, i32> {
    if frame.sequence != 1 { return Err(PROTOCOL); }
    match frame.kind {
        Kind::ApproveView => Ok(Role::Observe),
        Kind::ApproveControl => Ok(Role::RequestControl),
        _ => Err(PROTOCOL),
    }
}
fn progress(state: State) -> Kind {
    match state {
        State::Opening | State::Mapped => Kind::Ready,
        State::Allowed => Kind::Allowed,
        State::Denied => Kind::Denied,
    }
}
fn final_reply(
    command: &mut UnixStream,
    signals: &UnixDatagram,
    surface: &mut Surface,
    request: Frame,
    decision: Kind,
    until: Instant,
) -> i32 {
    surface.close();
    // Destroying native resources never gives a late positive decision new time.
    let decision = if decision == Kind::Allowed && (Instant::now() >= until || fenced(signals)) {
        Kind::Denied
    } else { decision };
    send(command, request, decision).map_or(CHANNEL, |()| {
        if decision == Kind::Allowed { 0 } else { REFUSED }
    })
}
pub(crate) fn serve(mut command: UnixStream, signals: UnixDatagram, hello: Frame) -> i32 {
    let role = match intent(hello) {
        Ok(role) => role,
        Err(error) => return error,
    };
    let until = Instant::now() + PROMPT_WAIT;
    let opened = std::env::var("DISPLAY").ok()
        .and_then(|display| Surface::open(&display, role, until).ok());
    let Some(mut surface) = opened else {
        let _ = send(&mut command, hello, Kind::Refused);
        return REFUSED;
    };
    let mut request = hello;
    let mut expected = 2_u64;
    loop {
        if Instant::now() >= until || fenced(&signals) {
            return final_reply(&mut command, &signals, &mut surface, request, Kind::Denied, until);
        }
        let state = match surface.poll() {
            Ok(state) => state,
            Err(_) => return final_reply(&mut command, &signals, &mut surface, request, Kind::Refused, until),
        };
        if state == State::Opening {
            // No Ready response until real native mapping. Parent opening and
            // original approval deadlines continue while this one child polls.
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let reply = progress(state);
        if reply != Kind::Ready {
            return final_reply(&mut command, &signals, &mut surface, request, reply, until);
        }
        if send(&mut command, request, Kind::Ready).is_err() { return CHANNEL; }
        request = match read(&mut command, IDLE_WAIT.min(until.saturating_duration_since(Instant::now()))) {
            Ok(next) if next.epoch == hello.epoch && next.sequence == expected => next,
            Ok(_) => return PROTOCOL,
            Err(error) => return error,
        };
        let Some(next) = expected.checked_add(1) else { return PROTOCOL; };
        expected = next;
        match request.kind {
            Kind::ApprovalCheck => {}
            Kind::Stop => {
                surface.close();
                return send(&mut command, request, Kind::Stopped).map_or(CHANNEL, |()| 0);
            }
            // Reopening, changing role, ordinary indicator polling and reply
            // frames all refuse; no old UI can approve a second request.
            _ => return PROTOCOL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(kind: Kind) -> Frame { Frame { kind, sequence: 1, epoch: 7 } }
    #[test]
    fn no_indication_or_reply_record_selects_a_consent_role() {
        for kind in [Kind::Open, Kind::Check, Kind::Stop, Kind::ApprovalCheck,
            Kind::Ready, Kind::Stopped, Kind::Refused, Kind::Allowed, Kind::Denied] {
            assert_eq!(intent(frame(kind)), Err(PROTOCOL));
        }
        assert_eq!(intent(frame(Kind::ApproveView)), Ok(Role::Observe));
        assert_eq!(intent(frame(Kind::ApproveControl)), Ok(Role::RequestControl));
        assert_eq!(intent(Frame { sequence: 2, ..frame(Kind::ApproveView) }), Err(PROTOCOL));
    }
    #[test]
    fn native_mapping_is_not_positive_consent() {
        assert_eq!(progress(State::Mapped), Kind::Ready);
        assert_eq!(progress(State::Denied), Kind::Denied);
        assert_eq!(progress(State::Allowed), Kind::Allowed);
    }
}
