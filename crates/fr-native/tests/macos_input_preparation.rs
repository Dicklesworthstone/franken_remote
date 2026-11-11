//! Executes the production Rust Quartz owner against a deterministic FFI test
//! boundary on non-macOS builders. These are submission-order/ownership tests,
//! not evidence of real Quartz posting, Accessibility or native input delivery.
#![cfg(not(target_os = "macos"))]
#![allow(dead_code)]

#[path = "../src/macos_display.rs"]
mod macos_display;
#[path = "../src/macos_input.rs"]
pub mod macos_input;
#[path = "../src/macos_input_state.rs"]
mod macos_input_state;
#[path = "../src/macos_keys.rs"]
mod macos_keys;

use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit},
    input_submission::{Capabilities, Operation, PlatformError, Submission, scroll::LINE},
};
use macos_input::MacPointer;
use std::{cell::RefCell, ffi::c_void};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Key,
    Move,
    Button,
    Scroll,
    Text,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Query,
    Construct(Kind),
    Check,
    Post(Kind),
    Free,
}
#[derive(Clone, Copy)]
struct Event {
    kind: Kind,
    point: (f64, f64),
    clicks: i64,
}
impl Event {
    const fn new(kind: Kind) -> Self {
        Self {
            kind,
            point: (0.0, 0.0),
            clicks: 0,
        }
    }
}
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // Independent foreign-boundary fault injections.
struct State {
    steps: Vec<Step>,
    posted: Vec<Event>,
    live: usize,
    now: u64,
    constructor_time: u64,
    fail_create: bool,
    fail_location: bool,
    post_result: i32,
    change_display_on_create: bool,
    display_changed: bool,
    position: (f64, f64),
}
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}
fn state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|s| f(&mut s.borrow_mut()))
}
fn query() {
    state(|s| s.steps.push(Step::Query));
}
fn construct(event: Event, out: *mut *mut c_void) -> i32 {
    state(|s| {
        s.steps.push(Step::Construct(event.kind));
        s.now += s.constructor_time;
        s.display_changed |= s.change_display_on_create;
        // SAFETY: the production owner supplies one writable out pointer.
        unsafe { *out = core::ptr::null_mut() };
        if s.fail_create {
            return -2;
        }
        s.live += 1;
        // SAFETY: ownership of this one allocation transfers to the owner;
        // fr_in_event_close below reclaims it exactly once.
        unsafe { *out = Box::into_raw(Box::new(event)).cast() };
        0
    })
}
fn check() -> Result<(), &'static str> {
    state(|s| {
        s.steps.push(Step::Check);
        if s.now >= 100 { Err("expired") } else { Ok(()) }
    })
}
fn open() -> MacPointer {
    state(|s| *s = State::default());
    let pointer = MacPointer::open_exact(
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1280, 720).unwrap(),
        Capabilities::default(),
    )
    .unwrap();
    state(|s| {
        s.steps.clear();
        assert_eq!(s.live, 0, "temporary text probe events must retire");
    });
    pointer
}
fn press() -> Operation {
    Operation::Key {
        key: PhysicalKey::new(0x04).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn positives() -> [Operation; 5] {
    [
        press(),
        Operation::Absolute(DesktopPoint { x: 20, y: 30 }),
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Operation::Scroll {
            x: LINE,
            y: -LINE,
            unit: ScrollUnit::Lines,
        },
        Operation::Scroll {
            x: LINE / 2,
            y: -LINE / 2,
            unit: ScrollUnit::Pixels,
        },
    ]
}

#[test]
fn every_native_constructor_finishes_before_the_original_deadline_check() {
    for op in positives() {
        let mut pointer = open();
        pointer.prepare(op).unwrap();
        state(|s| s.steps.clear());
        assert_eq!(
            pointer.submit_checked(op, |_| check()),
            Ok(Submission::Submitted)
        );
        state(|s| {
            let at = s.steps.iter().position(|s| *s == Step::Check).unwrap();
            assert!(
                s.steps[..at]
                    .iter()
                    .any(|s| matches!(s, Step::Construct(_)))
            );
            assert!(matches!(s.steps[at + 1], Step::Post(_)));
            assert!(
                !s.steps[at + 1..]
                    .iter()
                    .any(|s| matches!(s, Step::Query | Step::Construct(_)))
            );
            assert_eq!(s.posted.len(), 1);
            assert_eq!(s.live, 0);
        });
        drop(pointer);
    }
}

#[test]
fn stalled_native_construction_expires_without_posting_or_retaining_a_press() {
    for op in positives() {
        let mut pointer = open();
        pointer.prepare(op).unwrap();
        state(|s| s.constructor_time = 100);
        assert_eq!(pointer.submit_checked(op, |_| check()), Err("expired"));
        assert!(pointer.native_held().is_empty());
        assert_eq!(
            pointer.submit(op),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        drop(pointer);
        state(|s| {
            assert!(
                s.posted.is_empty(),
                "no event, including an invented cleanup release"
            );
            assert_eq!(s.live, 0);
        });
    }
}

#[test]
fn failed_constructor_is_not_an_uncertain_submission() {
    for op in positives() {
        let mut pointer = open();
        pointer.prepare(op).unwrap();
        state(|s| s.fail_create = true);
        assert_eq!(
            pointer.submit_checked(op, |_| panic!("no complete native event")),
            Ok::<_, ()>(Submission::NotSubmitted(PlatformError::Unavailable))
        );
        assert!(pointer.native_held().is_empty());
        drop(pointer);
        state(|s| {
            assert!(s.posted.is_empty());
            assert_eq!(s.live, 0);
        });
    }
}

#[test]
fn a_display_change_during_native_construction_refuses_the_original_target() {
    for op in positives() {
        let mut pointer = open();
        pointer.prepare(op).unwrap();
        state(|s| s.change_display_on_create = true);
        assert_eq!(
            pointer.submit_checked(op, |_| panic!("original native target changed")),
            Ok::<_, ()>(Submission::NotSubmitted(PlatformError::GeometryChanged))
        );
        assert!(pointer.native_held().is_empty());
        drop(pointer);
        state(|s| {
            assert!(s.posted.is_empty());
            assert_eq!(s.live, 0);
        });
    }
}

#[test]
fn confirmed_post_refusal_preserves_only_the_original_held_state() {
    let mut pointer = open();
    pointer.prepare(press()).unwrap();
    assert_eq!(
        pointer.submit_checked(press(), |_| check()),
        Ok(Submission::Submitted)
    );
    let original = pointer.native_held().words();
    let other = Operation::Key {
        key: PhysicalKey::new(0x05).unwrap(),
        transition: KeyTransition::Press,
    };
    pointer.prepare(other).unwrap();
    state(|s| s.post_result = -1);
    assert_eq!(
        pointer.submit_checked(other, |_| check()),
        Ok(Submission::NotSubmitted(PlatformError::Unsupported))
    );
    assert_eq!(pointer.native_held().words(), original);
    state(|s| {
        assert_eq!(s.posted.len(), 1);
        assert_eq!(s.live, 0);
        s.post_result = 0;
    });
    assert!(pointer.cleanup_native());
    state(|s| assert_eq!(s.posted.len(), 2, "only the original key needs a release"));
}

#[test]
fn uncertain_post_retains_its_pessimistic_cleanup_set() {
    let mut pointer = open();
    pointer.prepare(press()).unwrap();
    state(|s| s.post_result = -7);
    assert_eq!(
        pointer.submit_checked(press(), |_| check()),
        Ok(Submission::Unknown)
    );
    assert!(!pointer.native_held().is_empty());
    state(|s| s.post_result = 0);
    assert!(pointer.cleanup_native());
    state(|s| assert_eq!(s.posted.len(), 1, "unknown press still needs a release"));
}

#[test]
fn a_button_target_cannot_change_after_the_original_authority_check() {
    let mut pointer = open();
    let op = positives()[2];
    state(|s| s.position = (25.0, 40.0));
    pointer.prepare(op).unwrap();
    assert_eq!(
        pointer.submit_checked(op, |_| {
            // An independent local pointer change is not permission to retarget
            // an event whose original position has already been prepared.
            state(|s| s.position = (900.0, 600.0));
            check()
        }),
        Ok(Submission::Submitted)
    );
    state(|s| assert_eq!(s.posted[0].point, (25.0, 40.0)));
}

#[test]
fn missing_native_pointer_evidence_refuses_before_any_button_or_scroll() {
    for op in [positives()[2], positives()[3], positives()[4]] {
        let mut pointer = open();
        pointer.prepare(op).unwrap();
        state(|s| s.fail_location = true);
        assert_eq!(
            pointer.submit_checked(op, |_| panic!("pointer evidence unavailable")),
            Ok::<_, ()>(Submission::NotSubmitted(PlatformError::Unavailable))
        );
        assert!(pointer.native_held().is_empty());
        drop(pointer);
        state(|s| assert!(s.posted.is_empty()));
    }
}

#[test]
fn cancelled_button_preparation_does_not_consume_the_click_sequence() {
    let mut pointer = open();
    let op = positives()[2];
    pointer.prepare(op).unwrap();
    assert_eq!(
        pointer.submit_checked(op, |_| Err("revoked")),
        Err("revoked")
    );
    pointer.prepare(op).unwrap();
    assert_eq!(
        pointer.submit_checked(op, |_| check()),
        Ok(Submission::Submitted)
    );
    state(|s| assert_eq!(s.posted[0].clicks, 1));
}

#[test]
fn a_refused_release_keeps_held_state_until_unconditional_cleanup() {
    let mut pointer = open();
    pointer.prepare(press()).unwrap();
    assert_eq!(
        pointer.submit_checked(press(), |_| check()),
        Ok(Submission::Submitted)
    );
    let release = Operation::Key {
        key: PhysicalKey::new(0x04).unwrap(),
        transition: KeyTransition::Release,
    };
    pointer.prepare(release).unwrap();
    state(|s| s.constructor_time = 100);
    assert_eq!(pointer.submit_checked(release, |_| check()), Err("expired"));
    assert!(!pointer.native_held().is_empty());
    assert!(pointer.cleanup_native());
    state(|s| assert_eq!(s.posted.len(), 2));
}

// All symbols below are this test executable's fake foreign boundary. None
// can enter the shipping library or substitute for native platform evidence.
macro_rules! boundary {
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        // SAFETY: each symbol satisfies the private production FFI declaration
        // only inside this test executable; pointers have test-owned lifetimes.
        #[unsafe(no_mangle)]
        extern "C" fn $name($($arg: $ty),*) -> $ret $body
    };
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Display {
    id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    points_x: u32,
    points_y: u32,
    origin_x: f64,
    origin_y: f64,
}
boundary!(fr_in_access() -> i32 { query(); 1 });
boundary!(fr_in_request_access() -> i32 { query(); 1 });
boundary!(fr_in_display_check(_display: *const Display) -> i32 {
    query();
    state(|s| if s.display_changed { -5 } else { 0 })
});
boundary!(fr_in_displays(out: *mut Display, _capacity: usize, count: *mut usize) -> i32 {
    query();
    // SAFETY: the actual production caller supplies capacity 16 and count.
    unsafe {
        *out = Display { id: 1, x: 0, y: 0, width: 1280, height: 720,
                         points_x: 1280, points_y: 720, origin_x: 0.0, origin_y: 0.0 };
        *count = 1;
    }
    0
});
boundary!(fr_in_open() -> *mut c_void { Box::into_raw(Box::new(0_u8)).cast() });
boundary!(fr_in_close(source: *mut c_void) -> () {
    // SAFETY: this pointer is exactly the allocation returned by fr_in_open.
    unsafe { drop(Box::from_raw(source.cast::<u8>())) };
});
boundary!(fr_in_event_close(event: *mut c_void) -> () {
    if !event.is_null() {
        state(|s| { s.live -= 1; s.steps.push(Step::Free); });
        // SAFETY: constructor transfers one allocation to the production owner.
        unsafe { drop(Box::from_raw(event.cast::<Event>())) };
    }
});
boundary!(fr_in_event_post(event: *mut c_void) -> i32 {
    // SAFETY: the production owner borrows the original live Event allocation.
    let event = unsafe { *event.cast::<Event>() };
    state(|s| {
        if s.post_result == 0 {
            s.steps.push(Step::Post(event.kind));
            s.posted.push(event);
        }
        s.post_result
    })
});
boundary!(fr_in_key_prepare(_source: *mut c_void, _code: u16, _down: i32,
    _repeat: i32, _modifier: i32, _flags: u64, out: *mut *mut c_void) -> i32 {
    construct(Event::new(Kind::Key), out)
});
boundary!(fr_in_move_prepare(_source: *mut c_void, x: f64, y: f64,
    _held: i32, _flags: u64, out: *mut *mut c_void) -> i32 {
    query(); // Quartz obtains relative motion from current native position.
    if state(|s| s.fail_location) { return -2; }
    construct(Event { point: (x, y), ..Event::new(Kind::Move) }, out)
});
boundary!(fr_in_button_prepare(_source: *mut c_void, _button: i32,
    _pressed: i32, clicks: i64, x: f64, y: f64, _flags: u64, out: *mut *mut c_void) -> i32 {
    construct(Event { point: (x, y), clicks, ..Event::new(Kind::Button) }, out)
});
boundary!(fr_in_scroll_prepare(_source: *mut c_void, _dy: i32, _dx: i32,
    x: f64, y: f64, _pixels: i32, _flags: u64, out: *mut *mut c_void) -> i32 {
    construct(Event { point: (x, y), ..Event::new(Kind::Scroll) }, out)
});
boundary!(fr_in_location(x: *mut f64, y: *mut f64) -> i32 {
    query();
    state(|s| {
        if s.fail_location { return -2; }
        // SAFETY: the production caller supplies two writable f64 values.
        unsafe { *x = s.position.0; *y = s.position.1; }
        0
    })
});
boundary!(fr_in_double_click_interval() -> f64 { 0.5 });
boundary!(fr_in_record(_low: u64, _high: u64, _buttons: u32) -> () {});
boundary!(fr_in_key(source: *mut c_void, code: u16, down: i32, repeat: i32,
    modifier: i32, flags: u64) -> i32 {
    let mut event = core::ptr::null_mut();
    let result = fr_in_key_prepare(source, code, down, repeat, modifier, flags, &raw mut event);
    if result != 0 { return result; }
    let result = fr_in_event_post(event);
    fr_in_event_close(event);
    result
});
boundary!(fr_in_button(source: *mut c_void, button: i32, pressed: i32,
    clicks: i64, x: f64, y: f64, flags: u64) -> i32 {
    let mut event = core::ptr::null_mut();
    let result = fr_in_button_prepare(source, button, pressed, clicks, x, y, flags, &raw mut event);
    if result != 0 { return result; }
    let result = fr_in_event_post(event);
    fr_in_event_close(event);
    result
});
boundary!(fr_in_text_prepare(_source: *mut c_void, _units: *const u16,
    _count: usize, down: *mut *mut c_void, up: *mut *mut c_void) -> i32 {
    let first = construct(Event::new(Kind::Text), down);
    if first != 0 { return first; }
    construct(Event::new(Kind::Text), up)
});
boundary!(fr_in_text_post(down: *mut c_void, up: *mut c_void) -> i32 {
    fr_in_event_post(down);
    fr_in_event_post(up)
});
boundary!(fr_in_emergency_release() -> i32 { 0 });
boundary!(fr_in_bind_parent(_parent: i32) -> i32 { 0 });
boundary!(fr_in_route_to_process(_pid: i32) -> () {});
boundary!(fr_in_indicator_open(_display: u32, _timeout: u32) -> i32 { 0 });
boundary!(fr_in_clipboard_indicator_open(_timeout: u32) -> i32 { 0 });
boundary!(fr_in_indicator_pump(_timeout: u32) -> i32 { 0 });
boundary!(fr_in_indicator_close() -> () {});
