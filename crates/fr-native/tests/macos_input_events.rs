//! The macOS input owner's real Quartz events, read back without touching the
//! desktop (fr-macos-host-control-b14s). `route_to_process` sends every event
//! this process posts to one process only, where a per-process event tap
//! reads it: the owner's cursor, focus and applications never see one. What
//! this cannot cover is the HID-tap post itself (one call), qualified live
//! with the owner's OK.
//!
//! `harness = false`: a per-process tap needs this process registered with
//! the window server as an application (`NSApplicationLoad`), on the main
//! thread. Needs the Accessibility grant, like the agent tests.

#[cfg(not(all(target_os = "macos", feature = "macos-input")))]
fn main() {}

#[cfg(all(target_os = "macos", feature = "macos-input"))]
fn main() {
    quartz::main();
}

#[cfg(all(target_os = "macos", feature = "macos-input"))]
mod quartz {
    use core::ffi::c_void;
    use fr_core::{
        input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit},
        input_submission::{
            Capabilities, Capability, Operation, PlatformError, Submission, scroll::LINE,
        },
    };
    use fr_native::macos_input::{self, MacPointer};
    use std::{
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, Receiver, Sender},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    /// The source user data on every event the executor posts ("FRCP").
    const TAG: i64 = 0x4652_4350;

    // CGEventType.
    const LEFT_DOWN: u32 = 1;
    const LEFT_UP: u32 = 2;
    const RIGHT_DOWN: u32 = 3;
    const RIGHT_UP: u32 = 4;
    const MOVED: u32 = 5;
    const LEFT_DRAGGED: u32 = 6;
    const KEY_DOWN: u32 = 10;
    const KEY_UP: u32 = 11;
    const FLAGS_CHANGED: u32 = 12;
    const SCROLL: u32 = 22;
    const OTHER_DOWN: u32 = 25;
    const OTHER_UP: u32 = 26;
    /// Every mouse, key and scroll type the executor can post.
    const TYPES: [u32; 14] = [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 22, 25, 26, 27];

    // CGEventField.
    const CLICK_STATE: u32 = 1;
    const BUTTON_NUMBER: u32 = 3;
    const AUTOREPEAT: u32 = 8;
    const KEYCODE: u32 = 9;
    const AXIS_1: u32 = 11;
    const AXIS_2: u32 = 12;
    const USER_DATA: u32 = 42;
    const CONTINUOUS: u32 = 88;
    const FIXED_AXIS_1: u32 = 93;
    const FIXED_AXIS_2: u32 = 94;
    const POINT_AXIS_1: u32 = 96;
    const POINT_AXIS_2: u32 = 97;

    const SHIFT: u64 = 1 << 17;
    const LEFT_SHIFT_DEVICE: u64 = 0x2;
    const NON_COALESCED: u64 = 0x100;
    // Virtual keycodes.
    const VK_A: i64 = 0x00;
    const VK_B: i64 = 0x0b;
    const VK_SHIFT: i64 = 0x38;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    type TapCallback =
        unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreateForPid(
            pid: i32,
            place: u32,
            options: u32,
            mask: u64,
            callback: TapCallback,
            info: *mut c_void,
        ) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enable: bool);
        fn CGEventGetFlags(event: *mut c_void) -> u64;
        fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGEventKeyboardGetUnicodeString(
            event: *mut c_void,
            maximum: usize,
            actual: *mut usize,
            units: *mut u16,
        );
        fn CGDisplayBounds(display: u32) -> CGRect;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: *const c_void;
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: *mut c_void,
            order: isize,
        ) -> *mut c_void;
        fn CFMachPortInvalidate(port: *mut c_void);
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, once: u8) -> i32;
        fn CFRelease(object: *const c_void);
    }
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        /// Returns `BOOL`.
        fn NSApplicationLoad() -> i8;
    }

    /// One tagged event as this process received it.
    #[derive(Debug, Clone, Copy)]
    struct Seen {
        kind: u32,
        keycode: i64,
        autorepeat: i64,
        flags: u64,
        click: i64,
        button: i64,
        axis: (i64, i64),
        fixed: (i64, i64),
        points: (i64, i64),
        continuous: i64,
        at: (f64, f64),
        unicode: [u16; 2],
        unicode_length: usize,
    }

    unsafe extern "C" fn observe(
        _proxy: *mut c_void,
        kind: u32,
        event: *mut c_void,
        info: *mut c_void,
    ) -> *mut c_void {
        if event.is_null() || info.is_null() {
            return event;
        }
        // SAFETY: `event` is live for this callback; a scalar field read.
        let field = |f| unsafe { CGEventGetIntegerValueField(event, f) };
        // Only the executor's own events are read; anything else passes untouched.
        if field(USER_DATA) != TAG {
            return event;
        }
        // SAFETY: `event` is live for this callback.
        let (flags, at) = unsafe { (CGEventGetFlags(event), CGEventGetLocation(event)) };
        let (mut unicode, mut unicode_length) = ([0; 2], 0);
        if matches!(kind, KEY_DOWN | KEY_UP) {
            // SAFETY: the event is live for this callback and both outputs
            // are writable for the stated capacity. Fixture input only.
            unsafe {
                CGEventKeyboardGetUnicodeString(
                    event,
                    unicode.len(),
                    &raw mut unicode_length,
                    unicode.as_mut_ptr(),
                );
            }
        }
        let seen = Seen {
            kind,
            keycode: field(KEYCODE),
            autorepeat: field(AUTOREPEAT),
            flags,
            click: field(CLICK_STATE),
            button: field(BUTTON_NUMBER),
            axis: (field(AXIS_1), field(AXIS_2)),
            fixed: (field(FIXED_AXIS_1), field(FIXED_AXIS_2)),
            points: (field(POINT_AXIS_1), field(POINT_AXIS_2)),
            continuous: field(CONTINUOUS),
            at: (at.x, at.y),
            unicode,
            unicode_length,
        };
        // SAFETY: `info` is the tap thread's sender, alive until after the tap
        // is invalidated on that same thread, which is where this runs.
        let sender = unsafe { &*info.cast::<Sender<Seen>>() };
        let _ = sender.send(seen);
        event
    }

    /// A listen-only tap on this process's own event stream, on its own thread.
    struct Tap {
        events: Receiver<Seen>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Tap {
        fn open() -> Self {
            let (sender, events) = mpsc::channel();
            let (ready, opened) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let pid = i32::try_from(std::process::id()).unwrap();
                let mask: u64 = TYPES.iter().fold(0, |mask, &t| mask | (1 << t));
                let info = Box::into_raw(Box::new(sender));
                // SAFETY: `info` outlives the tap (released after invalidation
                // below); the callback matches the CGEventTapCallBack ABI.
                let tap = unsafe {
                    CGEventTapCreateForPid(pid, 0, 1, mask, observe, info.cast::<c_void>())
                };
                if tap.is_null() {
                    // SAFETY: never handed to a tap; reclaimed once.
                    drop(unsafe { Box::from_raw(info) });
                    ready.send(false).unwrap();
                    return;
                }
                // SAFETY: a live mach port; the run loop source and run loop are
                // this thread's, used only here.
                let source = unsafe {
                    let source = CFMachPortCreateRunLoopSource(core::ptr::null(), tap, 0);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
                    CGEventTapEnable(tap, true);
                    source
                };
                ready.send(true).unwrap();
                while !stopping.load(Ordering::Acquire) {
                    // SAFETY: runs this thread's own run loop briefly.
                    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 0) };
                }
                // SAFETY: the tap and source are released once, after which no
                // callback can run (they run only inside this thread's loop).
                unsafe {
                    CGEventTapEnable(tap, false);
                    CFMachPortInvalidate(tap);
                    CFRelease(source);
                    CFRelease(tap);
                    drop(Box::from_raw(info));
                }
            });
            assert!(
                opened.recv().unwrap(),
                "a per-process event tap needs the Accessibility grant"
            );
            Self {
                events,
                stop,
                thread: Some(thread),
            }
        }

        fn next(&self, step: &str) -> Seen {
            let seen = self
                .events
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("{step}: no event reached this process"));
            assert_eq!(
                seen.flags & NON_COALESCED,
                NON_COALESCED,
                "{step}: {seen:?}"
            );
            seen
        }

        /// Nothing further was posted.
        fn quiet(&self, step: &str) {
            if let Ok(seen) = self.events.recv_timeout(Duration::from_millis(150)) {
                panic!("{step}: unexpected {seen:?}");
            }
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn implemented() -> Capabilities {
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Repeat)
            .with(Capability::Absolute)
            .with(Capability::Buttons)
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll)
            .with(Capability::Text)
    }

    fn key(usage: u16, transition: KeyTransition) -> Operation {
        Operation::Key {
            key: PhysicalKey::new(usage).unwrap(),
            transition,
        }
    }

    fn button(button: PointerButton, pressed: bool) -> Operation {
        Operation::Button { button, pressed }
    }

    fn post(pointer: &mut MacPointer, step: &str, op: Operation) {
        if let Err(error) = pointer.prepare(op) {
            panic!("{step}: refused {error:?}");
        }
        assert_eq!(pointer.submit(op), Submission::Submitted, "{step}");
    }

    fn assert_at(step: &str, seen: Seen, at: (f64, f64)) {
        assert!(
            (seen.at.0 - at.0).abs() < 1e-6 && (seen.at.1 - at.1).abs() < 1e-6,
            "{step}: at {:?}, expected {at:?}",
            seen.at
        );
    }

    /// The first display a grant may name, its encoded rectangle and its
    /// rectangle in global points.
    fn display() -> (InputBounds, CGRect) {
        let (id, bounds) = *macos_input::display_bounds()
            .unwrap()
            .first()
            .expect("an active display");
        // SAFETY: a scalar query for a display id Core Graphics just listed.
        (bounds, unsafe { CGDisplayBounds(id) })
    }

    const CHILD: &str = "FR_INPUT_EVENTS_CHILD";

    pub fn main() {
        if std::env::var_os(CHILD).is_some() {
            panicking_child();
        }
        assert!(
            macos_input::access(),
            "grant Accessibility to the terminal running these tests"
        );
        // SAFETY: called once, on the main thread, before any AppKit use.
        assert_ne!(unsafe { NSApplicationLoad() }, 0);
        let tap = Tap::open();
        macos_input::route_to_process(std::process::id()).unwrap();
        let (bounds, points) = display();

        keys_carry_keycodes_repeat_and_modifier_flags(&tap, bounds);
        text_preserves_scalars_and_original_authority(&tap, bounds);
        pointer_events_land_on_the_mapped_points(&tap, bounds, points);
        refusals_post_nothing(&tap, bounds);
        cleanup_and_drop_release_what_is_held(&tap, bounds, points);
        a_panic_releases_the_recorded_set_and_exits(&tap);
        println!("macos_input_events: ok");
    }

    fn text_preserves_scalars_and_original_authority(tap: &Tap, bounds: InputBounds) {
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        for scalar in ['a', 'é', '\u{301}', '中', '🙂', '\u{10ffff}'] {
            let operation = Operation::Text(scalar);
            pointer.prepare(operation).unwrap();
            assert_eq!(
                pointer.submit_checked(operation, |held| {
                    assert!(held.key(fr_native::macos_input_state::TEXT_KEYCODE));
                    Ok::<(), ()>(())
                }),
                Ok(Submission::Submitted)
            );
            let down = tap.next("committed scalar");
            let up = tap.next("text cleanup");
            let mut expected = [0; 2];
            let expected = scalar.encode_utf16(&mut expected);
            assert_eq!(down.kind, KEY_DOWN);
            assert_eq!(down.unicode_length, expected.len());
            assert_eq!(&down.unicode[..down.unicode_length], expected);
            assert_eq!(down.flags, NON_COALESCED);
            assert_eq!(up.kind, KEY_UP);
            assert_eq!(up.keycode, down.keycode);
            assert_eq!(up.unicode_length, 0);
            assert!(pointer.native_held().is_empty());
        }
        // Text must not become Command-A or release the caller's held A.
        for usage in [0x04, 0xe3] {
            post(
                &mut pointer,
                "text collision key down",
                key(usage, KeyTransition::Press),
            );
            let _ = tap.next("text collision key down");
            assert_eq!(
                pointer.prepare(Operation::Text('é')),
                Err(PlatformError::Permission)
            );
            tap.quiet("text refused while a key was held");
            post(
                &mut pointer,
                "text collision key up",
                key(usage, KeyTransition::Release),
            );
            let _ = tap.next("text collision key up");
        }
        let operation = Operation::Text('🙂');
        pointer.prepare(operation).unwrap();
        assert_eq!(
            pointer.submit_checked(operation, |_| Err("expired")),
            Err("expired")
        );
        assert!(pointer.native_held().is_empty());
        assert_eq!(
            pointer.submit(operation),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        pointer.prepare(operation).unwrap();
        pointer.cancel_prepared();
        assert_eq!(
            pointer.submit(operation),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        pointer.prepare(operation).unwrap();
        assert_eq!(
            pointer.submit(Operation::Text('a')),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        assert_eq!(
            pointer.submit(operation),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        pointer.prepare(operation).unwrap();
        drop(pointer);
        tap.quiet("cancelled, changed, expired and abandoned text preparations");
    }

    fn keys_carry_keycodes_repeat_and_modifier_flags(tap: &Tap, bounds: InputBounds) {
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        let p = &mut pointer;
        post(p, "a down", key(0x04, KeyTransition::Press));
        let e = tap.next("a down");
        assert_eq!(
            (e.kind, e.keycode, e.autorepeat),
            (KEY_DOWN, VK_A, 0),
            "{e:?}"
        );
        assert_eq!(e.flags & SHIFT, 0, "{e:?}");
        post(p, "a repeat", key(0x04, KeyTransition::Repeat));
        let e = tap.next("a repeat");
        assert_eq!(
            (e.kind, e.keycode, e.autorepeat),
            (KEY_DOWN, VK_A, 1),
            "{e:?}"
        );
        post(p, "a up", key(0x04, KeyTransition::Release));
        let e = tap.next("a up");
        assert_eq!((e.kind, e.keycode), (KEY_UP, VK_A), "{e:?}");

        // A modifier is a flags change carrying the state after it, and keys
        // pressed under it carry it too.
        post(p, "shift down", key(0xe1, KeyTransition::Press));
        let e = tap.next("shift down");
        assert_eq!((e.kind, e.keycode), (FLAGS_CHANGED, VK_SHIFT), "{e:?}");
        assert_eq!(
            e.flags & (SHIFT | LEFT_SHIFT_DEVICE),
            SHIFT | LEFT_SHIFT_DEVICE
        );
        post(p, "shifted a down", key(0x04, KeyTransition::Press));
        let e = tap.next("shifted a down");
        assert_eq!((e.kind, e.keycode), (KEY_DOWN, VK_A), "{e:?}");
        assert_eq!(e.flags & SHIFT, SHIFT, "{e:?}");
        post(p, "shifted a up", key(0x04, KeyTransition::Release));
        assert_eq!(tap.next("shifted a up").kind, KEY_UP);
        post(p, "shift up", key(0xe1, KeyTransition::Release));
        let e = tap.next("shift up");
        assert_eq!((e.kind, e.keycode), (FLAGS_CHANGED, VK_SHIFT), "{e:?}");
        assert_eq!(e.flags & (SHIFT | LEFT_SHIFT_DEVICE), 0, "{e:?}");
        drop(pointer);
        tap.quiet("keys released before the owner closed");
    }

    fn pointer_events_land_on_the_mapped_points(tap: &Tap, bounds: InputBounds, points: CGRect) {
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        let p = &mut pointer;
        let origin = bounds.origin();
        let center = DesktopPoint {
            x: origin.x + i32::try_from(bounds.width() / 2).unwrap(),
            y: origin.y + i32::try_from(bounds.height() / 2).unwrap(),
        };
        let center_points = (
            points.origin.x + points.size.width / 2.0,
            points.origin.y + points.size.height / 2.0,
        );
        let origin_points = (points.origin.x, points.origin.y);

        // The encoded center is the display's center in points.
        post(p, "move to center", Operation::Absolute(center));
        let e = tap.next("move to center");
        assert_eq!(e.kind, MOVED, "{e:?}");
        assert_at("move to center", e, center_points);

        // Presses act where this owner placed the pointer; a second press
        // within the double-click interval is a double click.
        for clicks in 1..=2 {
            post(p, "left down", button(PointerButton::Primary, true));
            let e = tap.next("left down");
            assert_eq!((e.kind, e.click), (LEFT_DOWN, clicks), "{e:?}");
            assert_at("left down", e, center_points);
            post(p, "left up", button(PointerButton::Primary, false));
            let e = tap.next("left up");
            assert_eq!((e.kind, e.click), (LEFT_UP, clicks), "{e:?}");
            assert_at("left up", e, center_points);
        }

        // Somewhere else is a new click; a move while held is that button's drag.
        post(p, "move to origin", Operation::Absolute(origin));
        let e = tap.next("move to origin");
        assert_eq!(e.kind, MOVED, "{e:?}");
        assert_at("move to origin", e, origin_points);
        post(p, "drag down", button(PointerButton::Primary, true));
        let e = tap.next("drag down");
        assert_eq!((e.kind, e.click), (LEFT_DOWN, 1), "{e:?}");
        assert_at("drag down", e, origin_points);
        post(p, "drag", Operation::Absolute(center));
        let e = tap.next("drag");
        assert_eq!(e.kind, LEFT_DRAGGED, "{e:?}");
        assert_at("drag", e, center_points);
        post(p, "drop", button(PointerButton::Primary, false));
        let e = tap.next("drop");
        assert_eq!(e.kind, LEFT_UP, "{e:?}");
        assert_at("drop", e, center_points);

        post(p, "right down", button(PointerButton::Secondary, true));
        assert_eq!(tap.next("right down").kind, RIGHT_DOWN);
        post(p, "right up", button(PointerButton::Secondary, false));
        assert_eq!(tap.next("right up").kind, RIGHT_UP);
        post(p, "middle down", button(PointerButton::Middle, true));
        let e = tap.next("middle down");
        assert_eq!((e.kind, e.button), (OTHER_DOWN, 2), "{e:?}");
        post(p, "middle up", button(PointerButton::Middle, false));
        let e = tap.next("middle up");
        assert_eq!((e.kind, e.button), (OTHER_UP, 2), "{e:?}");

        // One line up and two right: Quartz counts up and left as positive.
        let scroll = Operation::Scroll {
            x: 2 * LINE,
            y: -LINE,
            unit: ScrollUnit::Lines,
        };
        post(p, "scroll", scroll);
        let e = tap.next("scroll");
        assert_eq!(
            (e.kind, e.axis, e.continuous),
            (SCROLL, (1, -2), 0),
            "{e:?}"
        );
        assert_at("scroll", e, center_points);
        precise_scroll_preserves_native_distance_and_authority(p, tap, center_points);
        drop(pointer);
        tap.quiet("nothing held when the owner closed");
    }

    fn precise_scroll_preserves_native_distance_and_authority(
        p: &mut MacPointer,
        tap: &Tap,
        center_points: (f64, f64),
    ) {
        // Native continuous scroll retains signed 16.16 fractions; its logical
        // distance is independent of this display's Retina backing scale.
        post(p, "scroll shift down", key(0xe1, KeyTransition::Press));
        assert_eq!(tap.next("scroll shift down").kind, FLAGS_CHANGED);
        let precise = Operation::Scroll {
            x: 3 * LINE / 2,
            y: -9 * LINE / 4,
            unit: ScrollUnit::Pixels,
        };
        post(p, "precise scroll", precise);
        let e = tap.next("precise scroll");
        assert_eq!((e.kind, e.continuous), (SCROLL, 1), "{e:?}");
        assert_eq!(
            e.fixed,
            (i64::from(9 * LINE / 4), i64::from(-3 * LINE / 2)),
            "{e:?}"
        );
        assert_eq!(e.points, (2, -1), "{e:?}");
        assert_ne!(e.flags & SHIFT, 0, "{e:?}");
        assert_at("precise scroll", e, center_points);
        post(p, "scroll shift up", key(0xe1, KeyTransition::Release));
        assert_eq!(tap.next("scroll shift up").kind, FLAGS_CHANGED);

        // A final authority refusal consumes preparation. No fresh ticket,
        // repeated call or cancellation can turn it into a later event.
        p.prepare(precise).unwrap();
        assert_eq!(
            p.submit_checked(precise, |_| Err("expired")),
            Err("expired")
        );
        assert_eq!(
            p.submit(precise),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        p.prepare(precise).unwrap();
        p.cancel_prepared();
        assert_eq!(
            p.submit(precise),
            Submission::NotSubmitted(PlatformError::Unsupported)
        );
        tap.quiet("expired and cancelled precise scrolls");
    }

    fn refusals_post_nothing(tap: &Tap, bounds: InputBounds) {
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        let outside = DesktopPoint {
            x: bounds.origin().x - 1,
            y: bounds.origin().y,
        };
        let refused = [
            (Operation::Absolute(outside), PlatformError::GeometryChanged),
            (
                key(0x04, KeyTransition::Release),
                PlatformError::Unsupported,
            ),
            (key(0x04, KeyTransition::Repeat), PlatformError::Unsupported),
            (
                button(PointerButton::Primary, false),
                PlatformError::Unsupported,
            ),
            (
                Operation::Scroll {
                    x: i32::MIN,
                    y: 0,
                    unit: ScrollUnit::Pixels,
                },
                PlatformError::Unsupported,
            ),
            (
                Operation::Scroll {
                    x: 0,
                    y: i32::MIN,
                    unit: ScrollUnit::Pixels,
                },
                PlatformError::Unsupported,
            ),
        ];
        for (op, error) in refused {
            assert_eq!(pointer.prepare(op), Err(error), "{op:?}");
            assert_eq!(
                pointer.submit(op),
                Submission::NotSubmitted(PlatformError::Unsupported),
                "an unprepared {op:?}"
            );
        }
        post(&mut pointer, "a down", key(0x04, KeyTransition::Press));
        assert_eq!(tap.next("a down").kind, KEY_DOWN);
        assert_eq!(
            pointer.prepare(key(0x04, KeyTransition::Press)),
            Err(PlatformError::Permission),
            "a held key is never pressed again"
        );
        post(&mut pointer, "a up", key(0x04, KeyTransition::Release));
        assert_eq!(tap.next("a up").kind, KEY_UP);
        // Modifiers never autorepeat, even while held.
        post(&mut pointer, "shift down", key(0xe1, KeyTransition::Press));
        assert_eq!(tap.next("shift down").kind, FLAGS_CHANGED);
        assert_eq!(
            pointer.prepare(key(0xe1, KeyTransition::Repeat)),
            Err(PlatformError::Unsupported)
        );
        post(&mut pointer, "shift up", key(0xe1, KeyTransition::Release));
        assert_eq!(tap.next("shift up").kind, FLAGS_CHANGED);
        tap.quiet("refusals");
    }

    fn cleanup_and_drop_release_what_is_held(tap: &Tap, bounds: InputBounds, points: CGRect) {
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        let center = (
            points.origin.x + points.size.width / 2.0,
            points.origin.y + points.size.height / 2.0,
        );
        let p = &mut pointer;
        let middle = DesktopPoint {
            x: bounds.origin().x + i32::try_from(bounds.width() / 2).unwrap(),
            y: bounds.origin().y + i32::try_from(bounds.height() / 2).unwrap(),
        };
        post(p, "move to center", Operation::Absolute(middle));
        assert_eq!(tap.next("move to center").kind, MOVED);
        post(p, "shift down", key(0xe1, KeyTransition::Press));
        assert_eq!(tap.next("shift down").kind, FLAGS_CHANGED);
        post(p, "b down", key(0x05, KeyTransition::Press));
        assert_eq!(tap.next("b down").keycode, VK_B);
        post(p, "left down", button(PointerButton::Primary, true));
        assert_eq!(tap.next("left down").kind, LEFT_DOWN);

        // Cleanup releases keys (the modifier as a flags change) and then the
        // button where this owner placed it, with no modifier left in flags.
        assert!(pointer.cleanup_native());
        let mut released: Vec<Seen> = (0..3).map(|_| tap.next("cleanup")).collect();
        released.sort_by_key(|e| e.kind);
        let [up, b, shift] = released[..] else {
            panic!("three releases")
        };
        assert_eq!(up.kind, LEFT_UP, "{up:?}");
        assert_at("cleanup", up, center);
        assert_eq!(up.flags & SHIFT, 0, "{up:?}");
        assert_eq!((b.kind, b.keycode), (KEY_UP, VK_B), "{b:?}");
        assert_eq!(
            (shift.kind, shift.keycode),
            (FLAGS_CHANGED, VK_SHIFT),
            "{shift:?}"
        );
        assert_eq!(shift.flags & SHIFT, 0, "{shift:?}");
        assert!(pointer.native_held().is_empty());
        assert!(
            pointer.cleanup_native(),
            "a second cleanup has nothing to do"
        );
        tap.quiet("cleanup posts each release once");

        // Dropping the owner releases what it still holds.
        post(&mut pointer, "a down", key(0x04, KeyTransition::Press));
        assert_eq!(tap.next("a down").kind, KEY_DOWN);
        drop(pointer);
        let e = tap.next("drop");
        assert_eq!((e.kind, e.keycode), (KEY_UP, VK_A), "{e:?}");
        tap.quiet("drop");
    }

    /// The panic path: a child holding shift and the left button, recorded,
    /// posts to THIS process and panics; its hook releases both and exits 70.
    fn a_panic_releases_the_recorded_set_and_exits(tap: &Tap) {
        let output = Command::new(std::env::current_exe().unwrap())
            .env(CHILD, std::process::id().to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(macos_input::EXIT_PANIC),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The child's own move and presses, then its last-resort releases.
        let mut seen: Vec<Seen> = (0..5).map(|_| tap.next("panic release")).collect();
        tap.quiet("one release per held input");
        let releases = seen.split_off(3);
        let kinds: Vec<u32> = seen.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [MOVED, FLAGS_CHANGED, LEFT_DOWN], "{seen:?}");
        assert!(
            releases
                .iter()
                .any(|e| e.kind == FLAGS_CHANGED && e.keycode == VK_SHIFT && e.flags & SHIFT == 0),
            "{releases:?}"
        );
        assert!(releases.iter().any(|e| e.kind == LEFT_UP), "{releases:?}");
    }

    fn panicking_child() -> ! {
        let parent: u32 = std::env::var(CHILD).unwrap().parse().unwrap();
        macos_input::route_to_process(parent).unwrap();
        macos_input::install_panic_release();
        let (bounds, _) = display();
        let mut pointer = MacPointer::open_exact(bounds, implemented()).unwrap();
        let center = DesktopPoint {
            x: bounds.origin().x + i32::try_from(bounds.width() / 2).unwrap(),
            y: bounds.origin().y + i32::try_from(bounds.height() / 2).unwrap(),
        };
        for op in [
            Operation::Absolute(center),
            key(0xe1, KeyTransition::Press),
            button(PointerButton::Primary, true),
        ] {
            pointer.prepare(op).unwrap();
            assert_eq!(pointer.submit(op), Submission::Submitted);
            macos_input::record(pointer.native_held());
        }
        // Never dropped: only the panic hook may release.
        std::mem::forget(pointer);
        panic!("an executor bug while input is held");
    }
}
