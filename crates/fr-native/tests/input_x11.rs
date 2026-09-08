#![cfg(all(target_os = "linux", feature = "linux-input"))]
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_native::input::X11Pointer;
use fr_wire::input::{
    InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input, encode_input,
};
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};
struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn time(n: u64) -> HostInstant {
    HostInstant::from_micros(n)
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn owner(sink: &X11Pointer) -> InputSession {
    // Explicit local test grant. This is NOT Tailscale identity/approval evidence.
    let c = credentials();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3000),
            ticket_lifetime: HostDuration::from_micros(1000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(time(0)).unwrap();
    a.mark_view_ready(time(0)).unwrap();
    a.grant_lease(c.lease, time(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, time(0)).unwrap();
    InputSession::new(a, c, sink.bounds(), sink.capabilities(), time(0)).unwrap()
}
fn send(
    owner: &mut InputSession,
    sink: &mut impl InputSink,
    credentials: InputCredentials,
    seq: u64,
    event: InputEvent<'_>,
    now: u64,
) -> Result<Dispatch, Refusal> {
    let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
    let route = if event.is_pointer() {
        InputDelivery::Datagram
    } else {
        InputDelivery::Reliable
    };
    let length = encode_input(
        InputRequest {
            credentials,
            sequence: seq,
            event,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        route,
    )
    .unwrap();
    let request = decode_input(
        &bytes[..length],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        route,
    )
    .unwrap();
    owner.dispatch(request, sink, || time(now))
}
fn receipt(result: Result<Dispatch, Refusal>) -> Receipt {
    let Dispatch::Completed(r) = result.unwrap() else {
        panic!("missing input receipt")
    };
    r
}
fn motion(x: i32, y: i32) -> InputEvent<'static> {
    InputEvent::Pointer {
        position: DesktopPoint { x, y },
    }
}
fn button(pressed: bool) -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed,
        position: DesktopPoint { x: 20, y: 30 },
        barrier: 21,
    }
}
#[test]
fn real_pointer_drag_barriers_duplicates_and_expiry_cleanup() {
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let mut o = owner(&sink);
    let c = credentials();
    assert_eq!(
        receipt(send(&mut o, &mut sink, c, 20, motion(10, 15), 10)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(
        sink.query_pointer().unwrap().0,
        DesktopPoint { x: 10, y: 15 }
    );
    let press = receipt(send(&mut o, &mut sink, c, 0, button(true), 10));
    assert_eq!(press.submitted_operations, 2);
    let (p, mask) = sink.query_pointer().unwrap();
    assert_eq!(p, DesktopPoint { x: 20, y: 30 });
    assert_ne!(mask & 256, 0);
    receipt(send(&mut o, &mut sink, c, 22, motion(70, 80), 10));
    assert_eq!(
        send(&mut o, &mut sink, c, 21, motion(1, 1), 10),
        Ok(Dispatch::ObsoletePointer)
    );
    assert_eq!(
        receipt(send(&mut o, &mut sink, c, 0, button(true), 10)),
        press
    );
    let (p, mask) = sink.query_pointer().unwrap();
    assert_eq!(p, DesktopPoint { x: 70, y: 80 });
    assert_ne!(mask & 256, 0);
    let r = receipt(send(&mut o, &mut sink, c, 1, button(false), 1000));
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_ne!(sink.query_pointer().unwrap().1 & 256, 0);
    assert_eq!(
        o.maintain(time(3000), &mut sink),
        Cleanup {
            submitted_releases: 1,
            remaining: 0
        }
    );
    assert_eq!(sink.query_pointer().unwrap().1 & 256, 0);
    assert!(send(&mut o, &mut sink, c, 2, button(true), 3001).is_err());
    assert_eq!(
        sink.query_pointer().unwrap().0,
        DesktopPoint { x: 70, y: 80 }
    );
}
#[test]
fn native_preparation_is_required_and_text_is_not_guessed() {
    for name in ["host:0", "localhost:0", ":0\0"] {
        assert!(X11Pointer::open(name).is_err());
    }
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let before = sink.query_pointer().unwrap();
    assert_eq!(
        sink.submit(Operation::Absolute(DesktopPoint { x: 10, y: 10 })),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(
        sink.prepare(Operation::Text('é')),
        Err(PlatformError::Unsupported)
    );
    let mut o = owner(&sink);
    let r = receipt(send(
        &mut o,
        &mut sink,
        credentials(),
        0,
        InputEvent::Text("é"),
        10,
    ));
    assert_eq!(r.refusal, Some(Refusal::Unsupported));
    assert_eq!(sink.query_pointer().unwrap(), before);
}
#[test]
fn stale_generation_and_off_display_input_do_not_move_the_server_pointer() {
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let mut o = owner(&sink);
    let before = sink.query_pointer().unwrap();
    let mut c = credentials();
    c.view.viewport = ViewportMappingGeneration::from_raw(2);
    assert_eq!(
        receipt(send(&mut o, &mut sink, c, 0, button(true), 10)).refusal,
        Some(Refusal::StaleView)
    );
    assert_eq!(sink.query_pointer().unwrap(), before);
    let mut o = owner(&sink);
    assert_eq!(
        receipt(send(
            &mut o,
            &mut sink,
            credentials(),
            0,
            motion(320, 0),
            10
        ))
        .refusal,
        Some(Refusal::OutOfBounds)
    );
    assert_eq!(sink.query_pointer().unwrap(), before);
}
struct RevokingSink {
    native: X11Pointer,
    revoke: RevokeHandle,
}
impl InputSink for RevokingSink {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.native.prepare(op)
    }
    fn submit(&mut self, op: Operation) -> Submission {
        let result = self.native.submit(op);
        self.revoke.revoke();
        result
    }
}
#[test]
fn revoke_after_actual_motion_prevents_the_following_button_press() {
    let display = Display::start();
    let native = X11Pointer::open(&display.name).unwrap();
    let mut o = owner(&native);
    let mut sink = RevokingSink {
        native,
        revoke: o.revoke_handle(),
    };
    let r = receipt(send(&mut o, &mut sink, credentials(), 0, button(true), 10));
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    let (p, mask) = sink.native.query_pointer().unwrap();
    assert_eq!(p, DesktopPoint { x: 20, y: 30 });
    assert_eq!(mask & 256, 0);
    assert_eq!(o.cleanup(&mut sink).remaining, 0);
}
