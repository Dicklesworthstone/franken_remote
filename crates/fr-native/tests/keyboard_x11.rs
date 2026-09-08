#![cfg(all(target_os = "linux", feature = "linux-input"))]
use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{InputLeaseId, InputTicketId, RemoteSessionId},
    input::{DesktopPoint, InputEvent, KeyTransition, PhysicalKey, PointerButton},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_core::{ids::*, input::*, input_sequence::InputOutcome, input_submission::*};
use fr_native::input::X11Pointer;
use fr_wire::input::{InputDelivery, InputDirection, decode_input, encode_input};
use std::{
    ffi::CString,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

// Independent Xlib observer, not a fake input backend. XEvent layout and all
// calls here follow Xlib.h; production code never uses these test declarations.
#[repr(C)]
#[derive(Clone, Copy)]
struct KeyEvent {
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
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    keycode: c_uint,
    same_screen: c_int,
}
#[repr(C)]
union Event {
    kind: c_int,
    key: KeyEvent,
    padding: [c_long; 24],
}
#[repr(C)]
struct KeyboardState {
    key_click_percent: c_int,
    bell_percent: c_int,
    bell_pitch: c_uint,
    bell_duration: c_uint,
    led_mask: c_ulong,
    global_auto_repeat: c_int,
    auto_repeats: [u8; 32],
}
unsafe extern "C" {
    fn XInitThreads() -> c_int;
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(d: *mut c_void) -> c_int;
    fn XDefaultRootWindow(d: *mut c_void) -> c_ulong;
    fn XCreateSimpleWindow(
        d: *mut c_void,
        parent: c_ulong,
        x: c_int,
        y: c_int,
        w: c_uint,
        h: c_uint,
        border: c_uint,
        border_pixel: c_ulong,
        bg: c_ulong,
    ) -> c_ulong;
    fn XMapWindow(d: *mut c_void, w: c_ulong) -> c_int;
    fn XSetInputFocus(d: *mut c_void, w: c_ulong, revert: c_int, t: c_ulong) -> c_int;
    fn XSelectInput(d: *mut c_void, w: c_ulong, mask: c_long) -> c_int;
    fn XSync(d: *mut c_void, discard: c_int) -> c_int;
    fn XKeysymToKeycode(d: *mut c_void, sym: c_ulong) -> u8;
    fn XQueryKeymap(d: *mut c_void, keys: *mut u8) -> c_int;
    fn XQueryPointer(
        d: *mut c_void,
        w: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        rx: *mut c_int,
        ry: *mut c_int,
        wx: *mut c_int,
        wy: *mut c_int,
        state: *mut c_uint,
    ) -> c_int;
    fn XPending(d: *mut c_void) -> c_int;
    fn XNextEvent(d: *mut c_void, e: *mut Event) -> c_int;
    fn XGetKeyboardControl(d: *mut c_void, state: *mut KeyboardState) -> c_int;
    fn XkbSetAutoRepeatRate(
        d: *mut c_void,
        device: c_uint,
        delay: c_uint,
        interval: c_uint,
    ) -> c_int;
    fn XChangeKeyboardMapping(
        d: *mut c_void,
        first: c_int,
        syms_per: c_int,
        syms: *mut c_ulong,
        num: c_int,
    ) -> c_int;
}
struct Server(Child);
impl Server {
    fn start() -> (Self, String) {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| unsafe {
            assert_ne!(XInitThreads(), 0);
        });
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
            .expect("install Xvfb for native input tests");
        let stdout = child.stdout.take().unwrap();
        let mut name = String::new();
        BufReader::new(stdout)
            .read_line(&mut name)
            .expect("Xvfb displayfd");
        assert!(!name.trim().is_empty(), "Xvfb must publish its display");
        (Self(child), format!(":{}", name.trim()))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Observer {
    d: *mut c_void,
    window: c_ulong,
    root: c_ulong,
}
impl Observer {
    fn new(name: &str) -> Self {
        let name = CString::new(name).unwrap();
        unsafe {
            let d = XOpenDisplay(name.as_ptr());
            assert!(!d.is_null());
            let root = XDefaultRootWindow(d);
            let window = XCreateSimpleWindow(d, root, 0, 0, 320, 240, 0, 0, 0);
            XSelectInput(d, window, 1 | 2 | 4 | 8 | 64);
            XMapWindow(d, window);
            XSync(d, 0);
            XSetInputFocus(d, window, 1, 0);
            XSync(d, 0);
            Self { d, window, root }
        }
    }
    fn code(&self, sym: c_ulong) -> u8 {
        unsafe { XKeysymToKeycode(self.d, sym) }
    }
    fn down(&self, code: u8) -> bool {
        let mut keys = [0; 32];
        unsafe {
            XQueryKeymap(self.d, keys.as_mut_ptr());
        }
        keys[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn wait_down(&self, code: u8, expected: bool) {
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while self.down(code) != expected && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(self.down(code), expected);
    }
    fn wait_pointer(&self, expected: (i32, i32, u32)) {
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while self.pointer() != expected && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(self.pointer(), expected);
    }
    fn pointer(&self) -> (i32, i32, u32) {
        let (mut root, mut child, mut rx, mut ry, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0, 0, 0);
        unsafe {
            assert_ne!(
                XQueryPointer(
                    self.d,
                    self.root,
                    &raw mut root,
                    &raw mut child,
                    &raw mut rx,
                    &raw mut ry,
                    &raw mut wx,
                    &raw mut wy,
                    &raw mut mask
                ),
                0
            );
        }
        (rx, ry, mask)
    }
    fn fast_repeat(&self) {
        unsafe {
            assert_ne!(XkbSetAutoRepeatRate(self.d, 0x100, 30, 10), 0);
            XSync(self.d, 0);
        }
    }
    fn repeat(&self, code: u8) -> bool {
        let mut s = KeyboardState {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led_mask: 0,
            global_auto_repeat: 0,
            auto_repeats: [0; 32],
        };
        unsafe {
            XGetKeyboardControl(self.d, &raw mut s);
        }
        s.auto_repeats[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn wait_events(&self, count: usize) -> Vec<(i32, u32, bool)> {
        let until = std::time::Instant::now() + Duration::from_secs(1);
        let mut seen = Vec::new();
        while seen.len() < count && std::time::Instant::now() < until {
            seen.extend(self.events());
            std::thread::sleep(Duration::from_millis(1));
        }
        seen.extend(self.events());
        seen
    }
    fn events(&self) -> Vec<(i32, u32, bool)> {
        let mut out = vec![];
        unsafe {
            XSync(self.d, 0);
            while XPending(self.d) > 0 {
                let mut e = Event { padding: [0; 24] };
                XNextEvent(self.d, &raw mut e);
                if [2, 3, 4, 5].contains(&e.kind) {
                    assert_eq!(e.key.window, self.window);
                    out.push((e.kind, e.key.keycode, e.key.send_event != 0));
                }
            }
        }
        out
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.d);
        }
    }
}
fn key(transition: KeyTransition) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn session(native: &X11Pointer) -> (InputSession, InputCredentials) {
    assert!(native.capabilities().contains(Capability::Keys));
    assert!(native.capabilities().contains(Capability::Repeat));
    let session = RemoteSessionId::from_raw(1);
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    let mut a = SessionAuthority::new(session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(HostInstant::ORIGIN).unwrap();
    a.mark_view_ready(HostInstant::ORIGIN).unwrap();
    a.grant_lease(lease, HostInstant::ORIGIN).unwrap();
    a.issue_input_ticket(lease, ticket, HostInstant::ORIGIN)
        .unwrap();
    let c = InputCredentials {
        session,
        lease,
        ticket,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    (
        InputSession::new(
            a,
            c,
            native.bounds(),
            native.capabilities(),
            HostInstant::ORIGIN,
        )
        .unwrap(),
        c,
    )
}
fn send(
    owner: &mut InputSession,
    sink: &mut impl InputSink,
    c: InputCredentials,
    sequence: u64,
    event: InputEvent<'_>,
    clock: impl FnMut() -> HostInstant,
) -> Dispatch {
    let mut packet = [0; 8192];
    let p = ProtocolLimits::ABSOLUTE;
    let n = encode_input(
        InputRequest {
            credentials: c,
            sequence,
            event,
        },
        &mut packet,
        &p,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let r = decode_input(
        &packet[..n],
        &p,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    owner.dispatch(r, sink, clock).unwrap()
}
fn completed(d: Dispatch) -> Receipt {
    match d {
        Dispatch::Completed(r) => r,
        _ => panic!("expected completed receipt"),
    }
}
#[test]
fn wire_to_authority_to_real_keys_repeat_pointer_and_cleanup() {
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let original = o.repeat(code);
    let mut native = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&native);
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    o.wait_down(code, true);
    assert!(!o.repeat(code));
    assert_eq!(o.wait_events(1), [(2, u32::from(code), false)]);
    o.fast_repeat();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(o.events(), [] as [(i32, u32, bool); 0]);
    let dup = completed(send(
        &mut owner,
        &mut native,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(dup, r);
    assert_eq!(o.events(), [] as [(i32, u32, bool); 0]);
    let repeat = completed(send(
        &mut owner,
        &mut native,
        c,
        1,
        key(KeyTransition::Repeat),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(repeat.submitted_operations, 2);
    assert_eq!(
        o.wait_events(2),
        [(3, u32::from(code), false), (2, u32::from(code), false)]
    );
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        2,
        key(KeyTransition::Release),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    o.wait_down(code, false);
    assert_eq!(o.repeat(code), original);
    o.events();
    let _ = send(
        &mut owner,
        &mut native,
        c,
        0,
        InputEvent::Pointer {
            position: DesktopPoint { x: 42, y: 71 },
        },
        || HostInstant::ORIGIN,
    );
    o.wait_pointer((42, 71, 0));
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        3,
        InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 51, y: 82 },
            barrier: 1,
        },
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.submitted_operations, 2);
    o.wait_pointer((51, 82, 1 << 8));
    let _ = send(
        &mut owner,
        &mut native,
        c,
        4,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    );
    owner.revoke_handle().revoke();
    let clean = owner.maintain(HostInstant::ORIGIN, &mut native);
    assert_eq!(clean.remaining, 0);
    assert_eq!(clean.submitted_releases, 2);
    o.wait_down(code, false);
    o.wait_pointer((51, 82, 0));
    assert_eq!(o.repeat(code), original);
    assert!(native.cleanup_keyboard());
}
#[test]
fn expired_after_xkb_preparation_never_presses_and_restores_repeat() {
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let original = o.repeat(code);
    let mut native = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&native);
    let mut checks = 0;
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        0,
        key(KeyTransition::Press),
        || {
            checks += 1;
            HostInstant::from_micros(if checks == 2 { 1_000_000 } else { 0 })
        },
    ));
    assert_eq!(checks, 2);
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(r.submitted_operations, 0);
    assert_eq!(owner.held_count(), 0);
    assert!(!o.down(code));
    assert_eq!(o.repeat(code), original);
    assert_eq!(o.events(), [] as [(i32, u32, bool); 0]);
    assert!(native.cleanup_keyboard());
}
#[test]
fn native_repeat_expiry_between_release_and_press_is_partial_not_replayed() {
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let original = o.repeat(code);
    let mut native = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&native);
    let _ = send(
        &mut owner,
        &mut native,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    );
    o.wait_events(1);
    let mut checks = 0;
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        1,
        key(KeyTransition::Repeat),
        || {
            checks += 1;
            HostInstant::from_micros(if checks == 3 { 1_000_000 } else { 0 })
        },
    ));
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(o.wait_events(1), [(3, u32::from(code), false)]);
    assert!(!o.down(code));
    assert_eq!(o.repeat(code), original);
    assert_eq!(owner.held_count(), 0);
    let dup = completed(send(
        &mut owner,
        &mut native,
        c,
        1,
        key(KeyTransition::Repeat),
        || HostInstant::from_micros(1_000_000),
    ));
    assert_eq!(dup, r);
    assert_eq!(o.events(), [] as [(i32, u32, bool); 0]);
}
#[test]
fn preexisting_key_press_is_not_taken_over_or_released() {
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let mut local = X11Pointer::open(&name).unwrap();
    let (mut local_owner, lc) = session(&local);
    let _ = send(
        &mut local_owner,
        &mut local,
        lc,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    );
    o.wait_down(code, true);
    let mut remote = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&remote);
    let r = completed(send(
        &mut owner,
        &mut remote,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::RejectedBeforeSubmission);
    assert_eq!(
        r.refusal,
        Some(Refusal::Platform(PlatformError::Permission))
    );
    assert_eq!(owner.cleanup(&mut remote).remaining, 0);
    drop(remote);
    assert!(o.down(code));
    assert_eq!(local_owner.cleanup(&mut local).remaining, 0);
    assert!(!o.down(code));
}
#[test]
fn remapped_characters_do_not_move_physical_press_or_held_release() {
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let mut native = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&native);
    let _ = send(
        &mut owner,
        &mut native,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    );
    o.wait_events(1);
    let mut symbol = 0x62;
    unsafe {
        XChangeKeyboardMapping(o.d, c_int::from(code), 1, &raw mut symbol, 1);
        XSync(o.d, 0);
    }
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        1,
        key(KeyTransition::Release),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert!(!o.down(code));
    assert_eq!(o.wait_events(1), [(3, u32::from(code), false)]);
    let r = completed(send(
        &mut owner,
        &mut native,
        c,
        2,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(o.wait_events(1), [(2, u32::from(code), false)]);
    owner.revoke();
    drop(native);
    assert!(!o.down(code));
}
#[test]
fn local_revoke_during_native_preparation_cancels_without_input() {
    struct RevokeAfterPrepare<'a> {
        native: &'a mut X11Pointer,
        revoke: RevokeHandle,
    }
    impl InputSink for RevokeAfterPrepare<'_> {
        fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
            self.native.prepare(op)?;
            self.revoke.revoke();
            Ok(())
        }
        fn submit(&mut self, op: Operation) -> Submission {
            self.native.submit(op)
        }
        fn cancel_prepared(&mut self) {
            self.native.cancel_prepared();
        }
    }
    let (_server, name) = Server::start();
    let o = Observer::new(&name);
    let code = o.code(0x61);
    let original = o.repeat(code);
    let mut native = X11Pointer::open(&name).unwrap();
    let (mut owner, c) = session(&native);
    let mut sink = RevokeAfterPrepare {
        native: &mut native,
        revoke: owner.revoke_handle(),
    };
    let r = completed(send(
        &mut owner,
        &mut sink,
        c,
        0,
        key(KeyTransition::Press),
        || HostInstant::ORIGIN,
    ));
    assert_eq!(r.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(r.submitted_operations, 0);
    assert!(!o.down(code));
    assert_eq!(o.repeat(code), original);
    assert_eq!(o.events(), [] as [(i32, u32, bool); 0]);
}
