#![cfg(all(
    target_os = "linux",
    feature = "linux-input",
    any(feature = "linux-displays", feature = "linux-input-monitors")
))]
//! Real `RandR` monitor selection and `XTest` input on a private Xvfb. Authority
//! and clock values are explicit fixtures; no tailnet or optical proof claimed.
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{scroll::WheelDirection, *},
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_native::{NativeError, input::X11Pointer};
use fr_wire::input::{
    InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input, encode_input,
};
use std::{
    ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void},
    ptr::NonNull,
};

#[path = "display_inventory/support.rs"]
#[allow(
    dead_code,
    reason = "shared native fixture also provides pixel painting for capture tests"
)]
mod support;
use support::Screen;

fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 320, y: 120 }, 320, 240).unwrap()
}
fn screen() -> Screen {
    let screen = Screen::start();
    screen.monitor("fr-left", 0, 0, 320, 480);
    screen.monitor("fr-selected", 320, 120, 320, 240);
    screen
}
fn selected(screen: &Screen) -> X11Pointer {
    // The original discovery is retained across native startup. Revalidation
    // after open models the caller's required causal pre-grant identity check.
    let mut discovery = screen.inventory();
    let catalog = discovery.catalog().unwrap();
    let descriptor = catalog.displays().iter().find(|d| d.x == 320).unwrap();
    let target = InputBounds::new(
        DesktopPoint {
            x: descriptor.x,
            y: descriptor.y,
        },
        descriptor.pixel_width,
        descriptor.pixel_height,
    )
    .unwrap();
    assert_eq!(target, bounds());
    let sink = X11Pointer::open_exact(&screen.name, target, Capabilities::default()).unwrap();
    discovery.revalidate().unwrap();
    assert_eq!(sink.bounds(), target);
    sink
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
fn time(n: u64) -> HostInstant {
    HostInstant::from_micros(n)
}
fn owner(sink: &X11Pointer) -> InputSession {
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
    sink: &mut X11Pointer,
    sequence: u64,
    event: InputEvent<'_>,
) -> Receipt {
    let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
    let delivery = if event.is_pointer() {
        InputDelivery::Datagram
    } else {
        InputDelivery::Reliable
    };
    let len = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence,
            event,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        delivery,
    )
    .unwrap();
    let parsed = decode_input(
        &bytes[..len],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        delivery,
    )
    .unwrap();
    let Dispatch::Completed(receipt) = owner.dispatch(parsed, sink, || time(10)).unwrap() else {
        panic!("missing terminal input receipt")
    };
    receipt
}
fn native(sink: &mut X11Pointer, operation: Operation) {
    sink.prepare(operation).unwrap();
    assert_eq!(sink.submit(operation), Submission::Submitted);
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
    padding: [c_long; 24],
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
    fn XWarpPointer(
        display: *mut c_void,
        source: c_ulong,
        destination: c_ulong,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        target_x: c_int,
        target_y: c_int,
    ) -> c_int;
    fn XQueryPointer(
        display: *mut c_void,
        window: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        root_x: *mut c_int,
        root_y: *mut c_int,
        x: *mut c_int,
        y: *mut c_int,
        mask: *mut c_uint,
    ) -> c_int;
    fn XQueryKeymap(display: *mut c_void, keys: *mut u8) -> c_int;
}
struct Observer(NonNull<c_void>);
impl Observer {
    fn new(name: &str) -> Self {
        let name = CString::new(name).unwrap();
        // SAFETY: valid local selector; this test thread owns the connection.
        let raw = NonNull::new(unsafe { XOpenDisplay(name.as_ptr()) }).unwrap();
        // SAFETY: the private server's root belongs to this connection. Only
        // button event selection, never a synthetic production input path.
        unsafe {
            XSelectInput(
                raw.as_ptr(),
                XDefaultRootWindow(raw.as_ptr()),
                (1 << 2) | (1 << 3),
            );
            XSync(raw.as_ptr(), 0);
        }
        Self(raw)
    }
    fn pointer(&self) -> (DesktopPoint, u32) {
        let (mut root, mut child, mut rx, mut ry, mut x, mut y, mut mask) = (0, 0, 0, 0, 0, 0, 0);
        // SAFETY: live private connection, distinct correctly sized outputs.
        unsafe {
            XSync(self.0.as_ptr(), 0);
            assert_ne!(
                XQueryPointer(
                    self.0.as_ptr(),
                    XDefaultRootWindow(self.0.as_ptr()),
                    &raw mut root,
                    &raw mut child,
                    &raw mut rx,
                    &raw mut ry,
                    &raw mut x,
                    &raw mut y,
                    &raw mut mask
                ),
                0
            );
        }
        (DesktopPoint { x: rx, y: ry }, mask)
    }
    fn warp(&self, point: DesktopPoint) {
        // SAFETY: independent local movement on the test-owned server; this
        // deliberately does not pass through the remote input owner's bounds.
        unsafe {
            XWarpPointer(
                self.0.as_ptr(),
                0,
                XDefaultRootWindow(self.0.as_ptr()),
                0,
                0,
                0,
                0,
                point.x,
                point.y,
            );
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn keys(&self) -> [u8; 32] {
        let mut keys = [0; 32];
        // SAFETY: full XQueryKeymap output, private live connection.
        unsafe {
            XQueryKeymap(self.0.as_ptr(), keys.as_mut_ptr());
        }
        keys
    }
    fn buttons(&self) -> Vec<(u32, bool, DesktopPoint)> {
        let mut result = Vec::new();
        // SAFETY: sole live observer, initialized XEvent-sized union. Read the
        // button layout only for the server's corresponding event discriminant.
        unsafe {
            XSync(self.0.as_ptr(), 0);
            while XPending(self.0.as_ptr()) > 0 {
                let mut event = Event { padding: [0; 24] };
                XNextEvent(self.0.as_ptr(), &raw mut event);
                if event.kind == 4 || event.kind == 5 {
                    let button = event.button;
                    result.push((
                        button.button,
                        event.kind == 4,
                        DesktopPoint {
                            x: button.root_x,
                            y: button.root_y,
                        },
                    ));
                    assert!(result.len() <= 32);
                }
            }
        }
        result
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: closes only this thread's uniquely owned observer connection.
        unsafe {
            XCloseDisplay(self.0.as_ptr());
        }
    }
}

#[test]
fn selected_nonzero_origin_reaches_real_pointer_button_and_scroll_coordinates() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    let mut owner = owner(&sink);
    let start = DesktopPoint { x: 321, y: 121 };
    let click = DesktopPoint { x: 400, y: 180 };
    let wheel = DesktopPoint { x: 520, y: 280 };
    let moved = send(
        &mut owner,
        &mut sink,
        0,
        InputEvent::Pointer { position: start },
    );
    assert_eq!(moved.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(observer.pointer().0, start);
    for (sequence, pressed) in [(0, true), (1, false)] {
        let result = send(
            &mut owner,
            &mut sink,
            sequence,
            InputEvent::Button {
                button: PointerButton::Primary,
                pressed,
                position: click,
                barrier: 1,
            },
        );
        assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
        assert_eq!(result.submitted_operations, 2);
        assert_eq!(observer.pointer().0, click);
        assert_eq!(observer.pointer().1 & 256 != 0, pressed);
    }
    let scroll = send(
        &mut owner,
        &mut sink,
        2,
        InputEvent::Scroll {
            position: wheel,
            barrier: 2,
            x: 1 << 16,
            y: -(1 << 16),
            unit: ScrollUnit::Lines,
        },
    );
    assert_eq!(scroll.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(scroll.submitted_operations, 5);
    assert_eq!(observer.pointer().0, wheel);
    assert_eq!(
        observer.buttons(),
        vec![
            (1, true, click),
            (1, false, click),
            (7, true, wheel),
            (7, false, wheel),
            (4, true, wheel),
            (4, false, wheel),
        ]
    );
}

#[test]
fn arbitrary_crops_and_ambiguous_matching_monitors_cannot_acquire_a_target() {
    let screen = screen();
    for target in [
        InputBounds::new(DesktopPoint { x: 321, y: 120 }, 318, 240).unwrap(),
        InputBounds::new(DesktopPoint { x: 160, y: 120 }, 320, 240).unwrap(),
        InputBounds::new(DesktopPoint { x: -1, y: 120 }, 320, 240).unwrap(),
    ] {
        assert_eq!(
            X11Pointer::open_exact(&screen.name, target, Capabilities::default()).err(),
            Some(PlatformError::GeometryChanged)
        );
    }
    screen.monitor("fr-ambiguous", 320, 120, 320, 240);
    assert_eq!(
        X11Pointer::open_exact(&screen.name, bounds(), Capabilities::default()).err(),
        Some(PlatformError::GeometryChanged)
    );
    // The unique default root-sized monitor is an exact selection too. It is
    // never used as a substitute for an ambiguous requested subset.
    let root = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 640, 480).unwrap();
    assert_eq!(
        X11Pointer::open_exact(&screen.name, root, Capabilities::default())
            .unwrap()
            .bounds(),
        root
    );
}

#[test]
fn native_and_wire_bounds_refuse_neighbor_pixels_without_clamping_or_rebasing() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    for point in [
        DesktopPoint { x: 320, y: 120 },
        DesktopPoint { x: 639, y: 359 },
    ] {
        native(&mut sink, Operation::Absolute(point));
        assert_eq!(observer.pointer().0, point);
    }
    let before = observer.pointer();
    for point in [
        DesktopPoint { x: 319, y: 120 },
        DesktopPoint { x: 320, y: 119 },
        DesktopPoint { x: 640, y: 120 },
        DesktopPoint { x: 320, y: 360 },
    ] {
        let operation = Operation::Absolute(point);
        assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
        assert_eq!(
            sink.submit(operation),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        let mut authority = owner(&sink);
        assert_eq!(
            send(
                &mut authority,
                &mut sink,
                0,
                InputEvent::Pointer { position: point }
            )
            .refusal,
            Some(Refusal::OutOfBounds)
        );
        assert_eq!(observer.pointer(), before);
    }
}

#[test]
fn local_pointer_escape_refuses_a_new_button_or_wheel_press() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    native(
        &mut sink,
        Operation::Absolute(DesktopPoint { x: 400, y: 180 }),
    );
    observer.warp(DesktopPoint { x: 20, y: 30 });
    for operation in [
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Operation::Button {
            button: PointerButton::Back,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Up,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Right,
            pressed: true,
        },
    ] {
        assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
        assert_eq!(
            sink.submit(operation),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
    }
    assert_eq!(observer.pointer().0, DesktopPoint { x: 20, y: 30 });
    assert_eq!(observer.pointer().1 & (256 | 512 | 1024 | 2048 | 4096), 0);
    assert_eq!(observer.buttons(), [] as [(u32, bool, DesktopPoint); 0]);
}

#[test]
fn remove_readd_of_equal_monitor_is_terminal_but_held_input_can_be_released() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    let key = PhysicalKey::new(0x04).unwrap();
    native(
        &mut sink,
        Operation::Absolute(DesktopPoint { x: 400, y: 180 }),
    );
    native(
        &mut sink,
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
    );
    native(
        &mut sink,
        Operation::Wheel {
            direction: WheelDirection::Up,
            pressed: true,
        },
    );
    native(
        &mut sink,
        Operation::Key {
            key,
            transition: KeyTransition::Press,
        },
    );
    assert_ne!(observer.pointer().1 & 256, 0);
    assert!(observer.keys().iter().any(|&byte| byte != 0));
    screen.remove("fr-selected");
    screen.monitor("fr-selected", 320, 120, 320, 240);
    for operation in [
        Operation::Absolute(DesktopPoint { x: 401, y: 181 }),
        Operation::Button {
            button: PointerButton::Secondary,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Right,
            pressed: true,
        },
        Operation::Key {
            key: PhysicalKey::new(0x05).unwrap(),
            transition: KeyTransition::Press,
        },
    ] {
        assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
    }
    // Cleanup is legal even after local movement leaves the selected monitor.
    observer.warp(DesktopPoint { x: 20, y: 30 });
    native(
        &mut sink,
        Operation::Button {
            button: PointerButton::Primary,
            pressed: false,
        },
    );
    native(
        &mut sink,
        Operation::Wheel {
            direction: WheelDirection::Up,
            pressed: false,
        },
    );
    native(
        &mut sink,
        Operation::Key {
            key,
            transition: KeyTransition::Release,
        },
    );
    assert_eq!(observer.pointer().1 & (256 | 2048), 0);
    assert_eq!(observer.keys(), [0; 32]);
    assert!(sink.cleanup_native());
    assert!(sink.native_held().is_empty());
    assert_eq!(
        sink.prepare(Operation::Absolute(DesktopPoint { x: 400, y: 180 })),
        Err(PlatformError::GeometryChanged)
    );
}

#[test]
fn changing_a_neighbor_fences_the_original_inventory_and_never_rebinds_it() {
    let screen = screen();
    let mut sink = selected(&screen);
    screen.remove("fr-left");
    screen.monitor("fr-left", 0, 0, 320, 480);
    let operation = Operation::Absolute(DesktopPoint { x: 400, y: 180 });
    assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
    assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
    let mut replacement = selected(&screen);
    native(&mut replacement, operation);
    assert_eq!(
        replacement.query_pointer().unwrap().0,
        DesktopPoint { x: 400, y: 180 }
    );
}

#[test]
fn original_discovery_cannot_confirm_a_replacement_that_opened_before_native_ready() {
    let screen = screen();
    let mut original = screen.inventory();
    let catalog = original.catalog().unwrap();
    screen.remove("fr-selected");
    screen.monitor("fr-selected", 320, 120, 320, 240);
    let replacement =
        X11Pointer::open_exact(&screen.name, bounds(), Capabilities::default()).unwrap();
    assert_eq!(replacement.bounds(), bounds());
    assert_eq!(screen.inventory().catalog().unwrap(), catalog);
    // Equal public descriptors are insufficient. The original source lifetime
    // sees the remove/re-add event, so the enclosing host must refuse its grant.
    assert_eq!(original.revalidate(), Err(NativeError::GeometryChanged));
    assert_eq!(original.revalidate(), Err(NativeError::Closed));
}

#[test]
fn topology_changed_after_preparation_refuses_submission_without_moving() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    observer.warp(DesktopPoint { x: 400, y: 180 });
    let operation = Operation::Absolute(DesktopPoint { x: 401, y: 181 });
    sink.prepare(operation).unwrap();
    screen.remove("fr-selected");
    screen.monitor("fr-selected", 320, 120, 320, 240);
    assert_eq!(
        sink.submit(operation),
        Submission::NotSubmitted(PlatformError::GeometryChanged)
    );
    assert_eq!(observer.pointer().0, DesktopPoint { x: 400, y: 180 });
    assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
}

#[test]
fn pointer_escape_after_preparation_refuses_button_and_wheel_submission() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    for operation in [
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Operation::Button {
            button: PointerButton::Back,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Up,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Right,
            pressed: true,
        },
    ] {
        observer.warp(DesktopPoint { x: 400, y: 180 });
        sink.prepare(operation).unwrap();
        observer.warp(DesktopPoint { x: 20, y: 30 });
        assert_eq!(
            sink.submit(operation),
            Submission::NotSubmitted(PlatformError::GeometryChanged)
        );
        assert_eq!(observer.pointer().0, DesktopPoint { x: 20, y: 30 });
        assert!(sink.native_held().is_empty());
    }
    assert_eq!(observer.buttons(), [] as [(u32, bool, DesktopPoint); 0]);
}

#[test]
fn final_authority_predicate_follows_revalidation_and_refusal_consumes_preparation() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut sink = selected(&screen);
    observer.warp(DesktopPoint { x: 400, y: 180 });
    let operation = Operation::Absolute(DesktopPoint { x: 401, y: 181 });
    sink.prepare(operation).unwrap();
    screen.remove("fr-selected");
    screen.monitor("fr-selected", 320, 120, 320, 240);
    let mut checked = false;
    assert_eq!(
        sink.submit_checked(operation, |_| {
            checked = true;
            Ok::<(), ()>(())
        }),
        Ok(Submission::NotSubmitted(PlatformError::GeometryChanged))
    );
    assert!(
        !checked,
        "invalid native evidence cannot reach the final authority predicate"
    );
    assert_eq!(observer.pointer().0, DesktopPoint { x: 400, y: 180 });

    let mut replacement = selected(&screen);
    let press = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };
    replacement.prepare(press).unwrap();
    assert_eq!(
        replacement.submit_checked(press, |held| {
            checked = true;
            assert!(
                !held.buttons.is_empty(),
                "the refreshed emergency release set precedes the final clock check"
            );
            Err(Submission::Expired)
        }),
        Err(Submission::Expired)
    );
    assert!(checked);
    assert_eq!(observer.pointer().1 & 256, 0);
    assert!(replacement.native_held().is_empty());
    assert_eq!(
        replacement.submit(press),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
}

#[test]
fn a_selected_monitor_covering_the_root_still_has_a_sticky_topology_lifetime() {
    let screen = Screen::start();
    let observer = Observer::new(&screen.name);
    let root = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 640, 480).unwrap();
    let mut sink = X11Pointer::open_exact(&screen.name, root, Capabilities::default()).unwrap();
    observer.warp(DesktopPoint { x: 400, y: 180 });
    let operation = Operation::Absolute(DesktopPoint { x: 401, y: 181 });
    sink.prepare(operation).unwrap();
    // The final monitor catalog and root dimensions are unchanged. Transient
    // topology is still a new lifetime, even for a full-root selected monitor.
    screen.monitor("fr-transient", 320, 120, 320, 240);
    screen.remove("fr-transient");
    assert_eq!(
        sink.submit(operation),
        Submission::NotSubmitted(PlatformError::GeometryChanged)
    );
    assert_eq!(observer.pointer().0, DesktopPoint { x: 400, y: 180 });
    assert_eq!(sink.prepare(operation), Err(PlatformError::GeometryChanged));
}

#[test]
fn ambiguous_full_root_monitor_selection_cannot_take_the_legacy_root_path() {
    let screen = Screen::start();
    let root = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 640, 480).unwrap();
    screen.monitor("fr-also-root", 0, 0, 640, 480);
    assert_eq!(
        X11Pointer::open_exact(&screen.name, root, Capabilities::default()).err(),
        Some(PlatformError::GeometryChanged)
    );
    // Explicit root-only instrumentation/probing retains its old API; only an
    // exact selected-monitor bind needs a unique native monitor identity.
    assert_eq!(X11Pointer::open(&screen.name).unwrap().bounds(), root);
}

#[path = "input_monitors_x11/ipc.rs"]
mod ipc;
