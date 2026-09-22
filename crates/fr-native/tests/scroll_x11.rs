#![cfg(all(target_os = "linux", feature = "linux-input"))]
//! Real X11 wheel events through production client codecs, native authority and
//! receipts. Admission, presentation and clocks are labeled local fixtures.
use fr_client::input::{
    Action, ClientInstant, InputClient, Policy, PresentedObservation, ResultEvent,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{
        scroll::{LINE, MAX_STEPS, WheelDirection},
        *,
    },
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_native::input::X11Pointer;
use fr_wire::{input::*, input_result::*};
use std::{
    cell::Cell,
    ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    ptr::NonNull,
};
struct Server {
    child: Child,
    name: String,
}
impl Server {
    fn new() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
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
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
struct ButtonEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    root: c_ulong,
    subwindow: c_ulong,
    time: c_ulong,
    x: c_int,
    y: c_int,
    root_x: c_int,
    root_y: c_int,
    state: c_uint,
    button: c_uint,
    same_screen: c_int,
}
#[repr(C)]
union Event {
    kind: c_int,
    button: ButtonEvent,
    pad: [c_long; 24],
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XSelectInput(display: *mut c_void, window: c_ulong, mask: c_long) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XPending(display: *mut c_void) -> c_int;
    fn XNextEvent(display: *mut c_void, event: *mut Event) -> c_int;
    fn XGetPointerMapping(display: *mut c_void, map: *mut u8, count: c_int) -> c_int;
    fn XSetPointerMapping(display: *mut c_void, map: *const u8, count: c_int) -> c_int;
}
struct Observer(NonNull<c_void>);
impl Observer {
    fn open(name: &str) -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let name = CString::new(name).unwrap();
        // SAFETY: local private display, valid CString, uniquely owned connection.
        let d = NonNull::new(unsafe { XOpenDisplay(name.as_ptr()) }).unwrap();
        // SAFETY: selected root belongs to d; masks are ButtonPress/ReleaseMask.
        unsafe {
            XSelectInput(
                d.as_ptr(),
                XDefaultRootWindow(d.as_ptr()),
                (1 << 2) | (1 << 3),
            );
            XSync(d.as_ptr(), 0);
        }
        Self(d)
    }
    fn events(&self) -> Vec<(u32, bool)> {
        let mut result = Vec::new();
        // SAFETY: this thread owns the live observer; sink roundtrip runs before
        // this drain. Read button fields only for corresponding initialized events.
        unsafe {
            XSync(self.0.as_ptr(), 0);
            while XPending(self.0.as_ptr()) > 0 {
                let mut event = Event { pad: [0; 24] };
                XNextEvent(self.0.as_ptr(), &raw mut event);
                if event.kind == 4 || event.kind == 5 {
                    let button = event.button;
                    assert_eq!((button.root_x, button.root_y), (20, 30));
                    result.push((button.button, event.kind == 4));
                    assert!(result.len() <= 2 * MAX_STEPS as usize + 8);
                }
            }
        }
        result
    }
    fn remap(&self, edit: impl FnOnce(&mut [u8])) {
        let mut map = [0; 256];
        // SAFETY: live owned display and full-sized pointer map buffer.
        let n = unsafe { XGetPointerMapping(self.0.as_ptr(), map.as_mut_ptr(), 256) };
        let n = usize::try_from(n).unwrap();
        assert!((1..=256).contains(&n));
        edit(&mut map[..n]);
        // SAFETY: same count and valid buffer; server validates the new map.
        unsafe {
            assert_eq!(
                XSetPointerMapping(self.0.as_ptr(), map.as_ptr(), c_int::try_from(n).unwrap()),
                0
            );
            XSync(self.0.as_ptr(), 0);
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: unique observer, no XTest extension cache was installed on it.
        unsafe {
            XCloseDisplay(self.0.as_ptr());
        }
    }
}
fn at(t: u64) -> HostInstant {
    HostInstant::from_micros(t)
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
fn grant(sink: &X11Pointer) -> (InputSession, InputClient) {
    grant_with_view_bound(sink, None)
}
fn grant_with_view_bound(
    sink: &X11Pointer,
    view_until: Option<u64>,
) -> (InputSession, InputClient) {
    let c = credentials();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3000),
            ticket_lifetime: HostDuration::from_micros(1000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    if let Some(until) = view_until {
        a.require_view_evidence(at(0)).unwrap();
        a.mark_view_ready_until(at(until), at(0)).unwrap();
    } else {
        a.mark_view_ready(at(0)).unwrap();
    }
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    let host = InputSession::new(a, c, sink.bounds(), sink.capabilities(), at(0)).unwrap();
    let mut client = InputClient::new(
        c,
        7,
        sink.bounds(),
        sink.capabilities(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    client
        .confirm_mapping(c.session, c.view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            PresentedObservation {
                session: c.session,
                serial: 0,
                view: c.view,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
    (host, client)
}
fn encode(client: &mut InputClient, x: i32, y: i32, t: u64) -> Vec<u8> {
    let mut bytes = vec![0; MAX_INPUT_RECORD_BYTES];
    let result = client
        .action(
            Action::Scroll {
                position: DesktopPoint { x: 20, y: 30 },
                x,
                y,
                unit: ScrollUnit::Lines,
            },
            &mut bytes,
            ClientInstant(t),
        )
        .unwrap();
    bytes.truncate(result.bytes);
    bytes
}
fn dispatch(
    host: &mut InputSession,
    sink: &mut impl InputSink,
    bytes: &[u8],
    clock: impl FnMut() -> HostInstant,
) -> Receipt {
    let request = decode_input(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let Dispatch::Completed(r) = host.dispatch(request, sink, clock).unwrap() else {
        panic!("missing receipt")
    };
    r
}
fn acknowledge(client: &mut InputClient, r: Receipt, t: u64) -> ResultEvent {
    let c = credentials();
    let result = InputResult::from_receipt(
        ResultBinding {
            channel: 7,
            session: c.session,
            lease: c.lease,
        },
        SequenceSpace::Action,
        r,
    )
    .unwrap();
    let mut bytes = [0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    client.result(&bytes[..n], ClientInstant(t)).unwrap()
}
fn barrier(sink: &mut X11Pointer) {
    assert_eq!(
        sink.query_pointer().unwrap().0,
        DesktopPoint { x: 20, y: 30 }
    );
}

fn setup() -> (Server, Observer, X11Pointer, InputSession, InputClient) {
    let server = Server::new();
    let observer = Observer::open(&server.name);
    let sink = X11Pointer::open(&server.name).unwrap();
    let (host, client) = grant(&sink);
    (server, observer, sink, host, client)
}

fn setup_view_bound(
    bound: Option<u64>,
) -> (Server, Observer, X11Pointer, InputSession, InputClient) {
    let server = Server::new();
    let observer = Observer::open(&server.name);
    let sink = X11Pointer::open(&server.name).unwrap();
    let (host, client) = grant_with_view_bound(&sink, bound);
    (server, observer, sink, host, client)
}

fn setup_native() -> (Server, Observer, X11Pointer) {
    let server = Server::new();
    let observer = Observer::open(&server.name);
    let sink = X11Pointer::open(&server.name).unwrap();
    (server, observer, sink)
}

#[test]
fn actual_four_direction_wheel_events_and_complete_client_receipts() {
    let (_server, observer, mut sink, mut host, mut client) = setup();
    assert!(sink.capabilities().contains(Capability::LineScroll));
    assert!(!sink.capabilities().contains(Capability::PixelScroll));
    for (i, (x, y, want)) in [
        (0, -LINE, vec![4]),
        (0, LINE, vec![5]),
        (-LINE, 0, vec![6]),
        (LINE, 0, vec![7]),
        (2 * LINE, -LINE, vec![7, 7, 4]),
    ]
    .into_iter()
    .enumerate()
    {
        let t = u64::try_from(i).unwrap() * 2 + 1;
        let bytes = encode(&mut client, x, y, t);
        let r = dispatch(&mut host, &mut sink, &bytes, || at(1));
        assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
        assert_eq!(
            r.submitted_operations,
            1 + 2 * u32::try_from(want.len()).unwrap()
        );
        assert!(matches!(
            acknowledge(&mut client, r, t + 1),
            ResultEvent::Completed(_)
        ));
        assert!(client.stopped().is_none());
        assert_eq!(host.held_count(), 0);
        barrier(&mut sink);
        let expected: Vec<_> = want
            .into_iter()
            .flat_map(|b| [(b, true), (b, false)])
            .collect();
        assert_eq!(observer.events(), expected);
    }
}
#[test]
fn replay_and_zero_scroll_cannot_generate_more_native_wheel_effects() {
    let (_server, observer, mut sink, mut host, mut client) = setup();
    let bytes = encode(&mut client, 0, 2 * LINE, 1);
    let r = dispatch(&mut host, &mut sink, &bytes, || at(1));
    assert!(matches!(
        acknowledge(&mut client, r, 2),
        ResultEvent::Completed(_)
    ));
    barrier(&mut sink);
    assert_eq!(
        observer.events(),
        [(5, true), (5, false), (5, true), (5, false)]
    );
    assert_eq!(dispatch(&mut host, &mut sink, &bytes, || at(2)), r);
    assert!(matches!(
        acknowledge(&mut client, r, 3),
        ResultEvent::Duplicate(_)
    ));
    barrier(&mut sink);
    assert_eq!(observer.events(), []);
    let zero = encode(&mut client, 0, 0, 4);
    let r = dispatch(&mut host, &mut sink, &zero, || at(3));
    assert_eq!(r.submitted_operations, 1);
    assert!(matches!(
        acknowledge(&mut client, r, 5),
        ResultEvent::Completed(_)
    ));
    assert!(client.stopped().is_none());
    barrier(&mut sink);
    assert_eq!(observer.events(), []);
}
#[test]
fn discrete_scroll_refuses_fractional_and_oversized_requests_before_moving() {
    for (x, y) in [
        (LINE / 2, 0),
        (0, -LINE / 4),
        (33 * LINE, 0),
        (16 * LINE, 17 * LINE),
        (i32::MIN, 0),
    ] {
        let (_server, observer, mut sink, mut host, mut client) = setup();
        let before = sink.query_pointer().unwrap();
        let bytes = encode(&mut client, x, y, 1);
        let r = dispatch(&mut host, &mut sink, &bytes, || at(1));
        assert_eq!(r.outcome, InputOutcome::RejectedBeforeSubmission);
        assert_eq!(r.submitted_operations, 0);
        assert_eq!(r.refusal, Some(Refusal::Unsupported));
        assert!(matches!(
            acknowledge(&mut client, r, 2),
            ResultEvent::Completed(_)
        ));
        assert!(client.stopped().is_some());
        assert_eq!(sink.query_pointer().unwrap(), before);
        assert_eq!(observer.events(), []);
    }
}
#[test]
fn remapped_wheel_uses_logical_direction_and_missing_mapping_never_becomes_a_click() {
    let server = Server::new();
    let observer = Observer::open(&server.name);
    observer.remap(|m| {
        m.swap(3, 4);
        m.swap(5, 6);
    });
    let mut sink = X11Pointer::open(&server.name).unwrap();
    let (mut host, mut client) = grant(&sink);
    let bytes = encode(&mut client, LINE, -LINE, 1);
    let r = dispatch(&mut host, &mut sink, &bytes, || at(1));
    assert_eq!(r.submitted_operations, 5);
    assert!(matches!(
        acknowledge(&mut client, r, 2),
        ResultEvent::Completed(_)
    ));
    barrier(&mut sink);
    assert_eq!(
        observer.events(),
        [(7, true), (7, false), (4, true), (4, false)]
    );
    observer.remap(|m| {
        let p = m.iter().position(|b| *b == 5).unwrap();
        m[p] = 0;
    });
    let fresh = X11Pointer::open(&server.name).unwrap();
    assert!(!fresh.capabilities().contains(Capability::LineScroll));
    let bytes = encode(&mut client, 0, LINE, 3);
    let r = dispatch(&mut host, &mut sink, &bytes, || at(2));
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    barrier(&mut sink);
    assert_eq!(observer.events(), []);
}
struct Hook<'a, F> {
    native: &'a mut X11Pointer,
    after: F,
}
impl<F: FnMut(Operation)> InputSink for Hook<'_, F> {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.native.prepare(op)
    }
    fn submit(&mut self, op: Operation) -> Submission {
        let result = self.native.submit(op);
        (self.after)(op);
        result
    }
    fn cancel_prepared(&mut self) {
        self.native.cancel_prepared();
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        true
    }
}
#[test]
fn expiry_after_native_press_stops_the_action_and_cleanup_releases_only_that_press() {
    let (_server, observer, mut sink, mut host, mut client) = setup();
    let clock = Cell::new(1);
    let bytes = encode(&mut client, 0, 3 * LINE, 1);
    let r = {
        let mut hook = Hook {
            native: &mut sink,
            after: |op| {
                if matches!(op, Operation::Wheel { pressed: true, .. }) {
                    clock.set(1000);
                }
            },
        };
        dispatch(&mut host, &mut hook, &bytes, || at(clock.get()))
    };
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 2);
    assert_eq!(host.held_count(), 1);
    assert!(matches!(
        acknowledge(&mut client, r, 2),
        ResultEvent::Completed(_)
    ));
    assert!(client.stopped().is_some());
    barrier(&mut sink);
    assert_eq!(observer.events(), [(5, true)]);
    let cleanup = host.cleanup(&mut sink);
    assert_eq!(cleanup.submitted_releases, 1);
    assert_eq!(cleanup.remaining, 0);
    barrier(&mut sink);
    assert_eq!(observer.events(), [(5, false)]);
    assert!(sink.cleanup_native());
    assert_eq!(dispatch(&mut host, &mut sink, &bytes, || at(1001)), r);
    barrier(&mut sink);
    assert_eq!(observer.events(), []);
}
#[test]
fn local_revoke_between_wheel_steps_prevents_another_press() {
    let (_server, observer, mut sink, mut host, mut client) = setup();
    let revoke = host.revoke_handle();
    let bytes = encode(&mut client, 0, 2 * LINE, 1);
    let r = {
        let mut hook = Hook {
            native: &mut sink,
            after: |op| {
                if matches!(op, Operation::Wheel { pressed: false, .. }) {
                    revoke.revoke();
                }
            },
        };
        dispatch(&mut host, &mut hook, &bytes, || at(1))
    };
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 3);
    assert_eq!(r.refusal, Some(Refusal::Revoked));
    assert_eq!(host.held_count(), 0);
    assert_eq!(host.cleanup(&mut sink).submitted_releases, 0);
    barrier(&mut sink);
    assert_eq!(observer.events(), [(5, true), (5, false)]);
}
#[test]
fn maximum_whole_line_request_stays_bounded_and_receipt_is_accepted() {
    let (_server, observer, mut sink, mut host, mut client) = setup();
    let bytes = encode(&mut client, 0, 32 * LINE, 1);
    let r = dispatch(&mut host, &mut sink, &bytes, || at(1));
    assert_eq!(r.submitted_operations, 65);
    assert!(matches!(
        acknowledge(&mut client, r, 2),
        ResultEvent::Completed(_)
    ));
    assert!(client.stopped().is_none());
    barrier(&mut sink);
    assert_eq!(observer.events().len(), 64);
    assert_eq!(host.held_count(), 0);
}
#[test]
fn dropping_a_prepared_native_wheel_owner_releases_its_recorded_button() {
    let (_server, observer, mut sink) = setup_native();
    let position = Operation::Absolute(DesktopPoint { x: 20, y: 30 });
    sink.prepare(position).unwrap();
    assert_eq!(sink.submit(position), Submission::Submitted);
    let press = Operation::Wheel {
        direction: WheelDirection::Up,
        pressed: true,
    };
    sink.prepare(press).unwrap();
    assert_eq!(sink.submit(press), Submission::Submitted);
    sink.cancel_prepared();
    barrier(&mut sink);
    assert_eq!(observer.events(), [(4, true)]);
    drop(sink);
    assert_eq!(observer.events(), [(4, false)]);
}

#[test]
fn existing_vertical_wheel_press_is_not_claimed_or_released_by_remote_cleanup() {
    let server = Server::new();
    let observer = Observer::open(&server.name);
    let mut local = X11Pointer::open(&server.name).unwrap();
    let mut remote = X11Pointer::open(&server.name).unwrap();
    let position = Operation::Absolute(DesktopPoint { x: 20, y: 30 });
    local.prepare(position).unwrap();
    assert_eq!(local.submit(position), Submission::Submitted);
    let pressed = Operation::Wheel {
        direction: WheelDirection::Up,
        pressed: true,
    };
    local.prepare(pressed).unwrap();
    assert_eq!(local.submit(pressed), Submission::Submitted);
    barrier(&mut local);
    assert_eq!(observer.events(), [(4, true)]);
    let (mut host, mut client) = grant(&remote);
    let bytes = encode(&mut client, 0, -LINE, 10);
    let result = dispatch(&mut host, &mut remote, &bytes, || at(10));
    assert_eq!(result.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(result.submitted_operations, 1);
    assert_eq!(
        result.refusal,
        Some(Refusal::Platform(PlatformError::Permission))
    );
    assert!(matches!(
        acknowledge(&mut client, result, 11),
        ResultEvent::Completed(_)
    ));
    assert_eq!(host.cleanup(&mut remote).remaining, 0);
    assert!(remote.cleanup_native());
    assert_ne!(remote.query_pointer().unwrap().1 & (1 << 11), 0);
    assert_eq!(observer.events(), []);
    assert!(local.cleanup_native());
    assert_eq!(local.query_pointer().unwrap().1 & (1 << 11), 0);
    assert_eq!(observer.events(), [(4, false)]);
}

#[test]
fn original_source_expiry_stops_native_scroll_even_when_ticket_and_lease_are_live() {
    for stop_after_press in [false, true] {
        let (_server, observer, mut sink, mut host, mut client) = setup_view_bound(Some(100));
        let clock = Cell::new(1);
        let bytes = encode(&mut client, LINE, LINE, 1);
        let monitor = host.monitor();
        assert_eq!(monitor.deadline(at(1)).unwrap(), at(100));
        let result = {
            let mut hook = Hook {
                native: &mut sink,
                after: |op| {
                    if matches!(op, Operation::Wheel { pressed, .. } if pressed == stop_after_press)
                    {
                        clock.set(100);
                    }
                },
            };
            dispatch(&mut host, &mut hook, &bytes, || at(clock.get()))
        };
        assert_eq!(result.outcome, InputOutcome::PartiallySubmittedToOs);
        assert_eq!(
            result.submitted_operations,
            if stop_after_press { 2 } else { 3 }
        );
        assert_eq!(
            result.refusal,
            Some(Refusal::Authority(
                fr_core::authority::AuthorityError::ViewUnready
            ))
        );
        assert_eq!(host.held_count(), u16::from(stop_after_press));
        assert!(matches!(
            acknowledge(&mut client, result, 2),
            ResultEvent::Completed(_)
        ));
        assert!(client.stopped().is_some());
        barrier(&mut sink);
        let expected = if stop_after_press {
            vec![(7, true)]
        } else {
            vec![(7, true), (7, false)]
        };
        assert_eq!(observer.events(), expected);
        let cleanup = host.maintain(at(100), &mut sink);
        assert_eq!(cleanup.remaining, 0);
        assert_eq!(cleanup.submitted_releases, u16::from(stop_after_press));
        barrier(&mut sink);
        assert_eq!(
            observer.events(),
            if stop_after_press {
                vec![(7, false)]
            } else {
                vec![]
            }
        );
        assert!(sink.cleanup_native());
        assert_eq!(dispatch(&mut host, &mut sink, &bytes, || at(101)), result);
        barrier(&mut sink);
        assert_eq!(observer.events(), []);
    }
}

#[test]
fn readiness_watchdog_revokes_a_prepared_native_wheel_before_submission() {
    struct Preparing<'a> {
        native: &'a mut X11Pointer,
        monitor: InputMonitor,
        clock: &'a Cell<u64>,
    }
    impl InputSink for Preparing<'_> {
        fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
            self.native.prepare(op)?;
            if matches!(op, Operation::Wheel { pressed: true, .. }) {
                self.clock.set(100);
                assert!(self.monitor.deadline(at(100)).is_err());
                assert!(self.monitor.is_revoked());
            }
            Ok(())
        }
        fn submit(&mut self, op: Operation) -> Submission {
            self.native.submit(op)
        }
        fn cancel_prepared(&mut self) {
            self.native.cancel_prepared();
        }
        fn line_scroll_requires_pairs(&self) -> bool {
            true
        }
    }
    let (_server, observer, mut sink, mut host, mut client) = setup_view_bound(Some(100));
    let clock = Cell::new(1);
    let bytes = encode(&mut client, 0, 3 * LINE, 1);
    let result = {
        let mut preparing = Preparing {
            native: &mut sink,
            monitor: host.monitor(),
            clock: &clock,
        };
        dispatch(&mut host, &mut preparing, &bytes, || at(clock.get()))
    };
    assert_eq!(result.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(result.submitted_operations, 1);
    assert_eq!(result.refusal, Some(Refusal::Revoked));
    assert_eq!(host.held_count(), 0);
    assert!(matches!(
        acknowledge(&mut client, result, 2),
        ResultEvent::Completed(_)
    ));
    assert_eq!(
        sink.submit(Operation::Wheel {
            direction: WheelDirection::Down,
            pressed: true
        }),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(host.cleanup(&mut sink).remaining, 0);
    barrier(&mut sink);
    assert_eq!(observer.events(), []);
}
