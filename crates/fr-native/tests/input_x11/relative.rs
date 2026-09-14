//! Real `XTest` displacement through production input parsing and authority.
//! Admission, view readiness and host-clock samples are explicit local fixtures.
use super::*;
use fr_core::authority::AuthorityError;
use std::{
    cell::Cell,
    ffi::{CString, c_char, c_int, c_ulong, c_void},
};

fn relative(x: i64, y: i64) -> InputEvent<'static> {
    InputEvent::Relative {
        mode_epoch: 1,
        cumulative_x: x,
        cumulative_y: y,
    }
}
fn enter_relative(sink: &mut X11Pointer) -> (InputSession, InputCredentials) {
    assert!(sink.capabilities().contains(Capability::Relative));
    let mut o = owner(sink);
    let c = credentials();
    assert_eq!(
        receipt(send(&mut o, sink, c, 0, motion(100, 100), 10)).outcome,
        InputOutcome::SubmittedToOs
    );
    let change = receipt(send(
        &mut o,
        sink,
        c,
        0,
        InputEvent::Mode {
            mode: PointerMode::Relative,
            epoch: 1,
        },
        20,
    ));
    assert_eq!(change.outcome, InputOutcome::AppliedLocally);
    assert_eq!(change.submitted_operations, 0);
    let ticket = InputTicketId::from_raw(4);
    assert_eq!(o.issue_ticket(ticket, time(20)).unwrap(), time(1020));
    (o, o_credentials(c, ticket))
}
fn o_credentials(mut c: InputCredentials, ticket: InputTicketId) -> InputCredentials {
    c.ticket = ticket;
    c
}
fn position(sink: &mut X11Pointer) -> DesktopPoint {
    sink.query_pointer().unwrap().0
}

#[test]
fn cumulative_relative_records_move_once_and_preserve_zero_and_reverse_steps() {
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let (mut o, c) = enter_relative(&mut sink);
    let first = receipt(send(&mut o, &mut sink, c, 1, relative(20, -10), 21));
    assert_eq!(first.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(first.submitted_operations, 1);
    assert_eq!(position(&mut sink), DesktopPoint { x: 120, y: 90 });
    assert_eq!(
        receipt(send(&mut o, &mut sink, c, 1, relative(20, -10), 22)),
        first
    );
    assert_eq!(position(&mut sink), DesktopPoint { x: 120, y: 90 });
    let same = receipt(send(&mut o, &mut sink, c, 2, relative(20, -10), 23));
    assert_eq!(same.outcome, InputOutcome::AppliedLocally);
    assert_eq!(same.submitted_operations, 0);
    let reverse = receipt(send(&mut o, &mut sink, c, 3, relative(7, 5), 24));
    assert_eq!(reverse.submitted_operations, 1);
    assert_eq!(position(&mut sink), DesktopPoint { x: 107, y: 105 });
    let ticket = InputTicketId::from_raw(5);
    o.issue_ticket(ticket, time(25)).unwrap();
    let c = o_credentials(c, ticket);
    assert_eq!(
        receipt(send(&mut o, &mut sink, c, 1, relative(20, -10), 26)),
        first
    );
    receipt(send(&mut o, &mut sink, c, 4, relative(9, 8), 27));
    assert_eq!(position(&mut sink), DesktopPoint { x: 109, y: 108 });
}

#[test]
fn relative_mode_requires_a_new_ticket_and_rejects_absolute_events() {
    for old_ticket in [true, false] {
        let display = Display::start();
        let mut sink = X11Pointer::open(&display.name).unwrap();
        let (mut o, c) = enter_relative(&mut sink);
        let r = if old_ticket {
            receipt(send(
                &mut o,
                &mut sink,
                credentials(),
                1,
                relative(10, 10),
                21,
            ))
        } else {
            receipt(send(&mut o, &mut sink, c, 1, motion(10, 10), 21))
        };
        assert_eq!(r.submitted_operations, 0);
        assert!(r.refusal.is_some());
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
        assert!(
            o.issue_ticket(InputTicketId::from_raw(6), time(22))
                .is_err()
        );
    }
}

#[test]
fn relative_mode_epochs_view_generations_and_reliable_gaps_are_fenced() {
    for case in 0..3 {
        let display = Display::start();
        let mut sink = X11Pointer::open(&display.name).unwrap();
        let (mut o, mut c) = enter_relative(&mut sink);
        let event = if case == 0 {
            InputEvent::Relative {
                mode_epoch: 0,
                cumulative_x: 10,
                cumulative_y: 10,
            }
        } else {
            relative(10, 10)
        };
        if case == 1 {
            c.view.geometry = DisplayGeometryGeneration::from_raw(2);
        }
        let result = send(
            &mut o,
            &mut sink,
            c,
            if case == 2 { 2 } else { 1 },
            event,
            21,
        );
        if case == 2 {
            assert!(matches!(result, Err(Refusal::Sequence(_))));
        } else {
            let r = receipt(result);
            assert_eq!(r.submitted_operations, 0);
            assert_eq!(
                r.refusal,
                Some(if case == 0 {
                    Refusal::ModeMismatch
                } else {
                    Refusal::StaleView
                })
            );
        }
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
    }
}

#[test]
fn relative_deltas_never_wrap_native_int16_or_clamp_known_off_display_targets() {
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let (_o, _c) = enter_relative(&mut sink);
    for (x, y) in [(32768, 0), (-32769, 0), (0, i32::MAX), (0, i32::MIN)] {
        assert_eq!(
            sink.prepare(Operation::Relative { x, y }),
            Err(PlatformError::Unsupported)
        );
        assert_eq!(
            sink.submit(Operation::Relative { x, y }),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
    }
    for (x, y) in [
        (220, 0),
        (-101, 0),
        (0, 140),
        (0, -101),
        (32767, 0),
        (-32768, 0),
    ] {
        assert_eq!(
            sink.prepare(Operation::Relative { x, y }),
            Err(PlatformError::GeometryChanged)
        );
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
    }
    sink.prepare(Operation::Relative { x: -100, y: -100 })
        .unwrap();
    assert_eq!(
        sink.submit(Operation::Relative { x: -100, y: -100 }),
        Submission::Submitted
    );
    assert_eq!(position(&mut sink), DesktopPoint { x: 0, y: 0 });
    sink.prepare(Operation::Relative { x: 319, y: 239 })
        .unwrap();
    assert_eq!(
        sink.submit(Operation::Relative { x: 319, y: 239 }),
        Submission::Submitted
    );
    assert_eq!(position(&mut sink), DesktopPoint { x: 319, y: 239 });
}

struct AfterPrepare<'a, F> {
    native: &'a mut X11Pointer,
    hook: F,
}
impl<F: FnMut()> InputSink for AfterPrepare<'_, F> {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.native.prepare(op)?;
        (self.hook)();
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.native.submit(op)
    }
    fn cancel_prepared(&mut self) {
        self.native.cancel_prepared();
    }
}
#[test]
fn expiry_and_local_revoke_after_native_preparation_prevent_any_relative_submission() {
    for expire in [false, true] {
        let display = Display::start();
        let mut sink = X11Pointer::open(&display.name).unwrap();
        let (mut o, c) = enter_relative(&mut sink);
        let clock = Cell::new(21);
        let revoke = o.revoke_handle();
        let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
        let len = encode_input(
            InputRequest {
                credentials: c,
                sequence: 1,
                event: relative(10, 10),
            },
            &mut bytes,
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let request = decode_input(
            &bytes[..len],
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let r = {
            let mut gate = AfterPrepare {
                native: &mut sink,
                hook: || {
                    if expire {
                        clock.set(1020);
                    } else {
                        revoke.revoke();
                    }
                },
            };
            receipt(o.dispatch(request, &mut gate, || time(clock.get())))
        };
        assert_eq!(r.submitted_operations, 0);
        if expire {
            assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
            assert_eq!(
                r.refusal,
                Some(Refusal::Authority(AuthorityError::TicketExpired))
            );
        } else {
            assert_eq!(r.refusal, Some(Refusal::Revoked));
        }
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
        assert_eq!(
            sink.submit(Operation::Relative { x: 10, y: 10 }),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        assert_eq!(receipt(o.dispatch(request, &mut sink, || time(1021))), r);
        assert_eq!(position(&mut sink), DesktopPoint { x: 100, y: 100 });
    }
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XWarpPointer(
        display: *mut c_void,
        src: c_ulong,
        dest: c_ulong,
        x: c_int,
        y: c_int,
        width: u32,
        height: u32,
        dest_x: c_int,
        dest_y: c_int,
    ) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
}
fn independent_local_motion(name: &str, x: i32, y: i32) {
    let name = CString::new(name).unwrap();
    // SAFETY: this test owns the independent display for the complete sequence;
    // all requests are scalar-only and retain no Rust pointer. No XTest state is
    // installed on this connection, and the server is the private test Xvfb.
    unsafe {
        let display = XOpenDisplay(name.as_ptr());
        assert!(!display.is_null());
        XWarpPointer(display, 0, XDefaultRootWindow(display), 0, 0, 0, 0, x, y);
        XSync(display, 0);
        XCloseDisplay(display);
    }
}
#[test]
fn prepared_relative_motion_uses_live_pointer_not_a_stale_absolute_warp() {
    let display = Display::start();
    let mut sink = X11Pointer::open(&display.name).unwrap();
    let (mut o, c) = enter_relative(&mut sink);
    let mut gate = AfterPrepare {
        native: &mut sink,
        hook: || independent_local_motion(&display.name, 130, 110),
    };
    let r = receipt(send(&mut o, &mut gate, c, 1, relative(10, 4), 21));
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(position(&mut sink), DesktopPoint { x: 140, y: 114 });
    let op = Operation::Relative { x: 1, y: 1 };
    sink.prepare(op).unwrap();
    sink.cancel_prepared();
    assert_eq!(
        sink.submit(op),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(position(&mut sink), DesktopPoint { x: 140, y: 114 });
}

#[test]
fn multiple_x_screens_cannot_enable_unbound_relative_input() {
    let mut child = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "320x240x24",
            "-screen",
            "1",
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
    let display = Display {
        child,
        name: format!(":{}", number.trim().parse::<u16>().unwrap()),
    };
    let mut sinks =
        [0, 1].map(|screen| X11Pointer::open(&format!("{}.{screen}", display.name)).unwrap());
    for sink in &mut sinks {
        assert!(!sink.capabilities().contains(Capability::Relative));
        assert!(sink.capabilities().contains(Capability::Absolute));
        assert_eq!(
            sink.prepare(Operation::Relative { x: 1, y: 1 }),
            Err(PlatformError::Unsupported)
        );
    }
}
