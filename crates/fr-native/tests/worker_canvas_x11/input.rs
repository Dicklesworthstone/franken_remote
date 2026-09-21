//! X-server routing only, not remote-control authority or physical input evidence.
use super::{Observer, Server, X11Surface, pattern};
use core::ffi::{c_int, c_long, c_uint, c_ulong, c_void};
use fr_core::limits::ProtocolLimits;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
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
    detail: c_uint,
    same_screen: c_int,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct VisibilityEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    state: c_int,
}
#[repr(C)]
union Event {
    kind: c_int,
    input: InputEvent,
    visibility: VisibilityEvent,
    padding: [c_long; 24],
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XSelectInput(display: *mut c_void, window: c_ulong, mask: c_long) -> c_int;
    fn XSetInputFocus(d: *mut c_void, focus: c_ulong, revert: c_int, time: c_ulong) -> c_int;
    fn XGetInputFocus(d: *mut c_void, focus: *mut c_ulong, revert: *mut c_int) -> c_int;
    fn XWarpPointer(
        d: *mut c_void,
        source: c_ulong,
        destination: c_ulong,
        source_x: c_int,
        source_y: c_int,
        source_width: c_uint,
        source_height: c_uint,
        destination_x: c_int,
        destination_y: c_int,
    ) -> c_int;
    fn XKeysymToKeycode(d: *mut c_void, keysym: c_ulong) -> u8;
    fn XSync(d: *mut c_void, discard: c_int) -> c_int;
    fn XPending(d: *mut c_void) -> c_int;
    fn XNextEvent(d: *mut c_void, event: *mut Event) -> c_int;
}
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestFakeButtonEvent(d: *mut c_void, button: c_uint, down: c_int, delay: c_ulong) -> c_int;
    fn XTestFakeKeyEvent(d: *mut c_void, key: c_uint, down: c_int, delay: c_ulong) -> c_int;
}

#[test]
fn private_canvas_preserves_original_focus_input_coordinates_and_visibility_events() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let d = observer.0.as_ptr();
    // SAFETY: one test-only connection selects input on its own fixture window.
    // No global/root event selection or input grab. The focus/time are local.
    unsafe {
        XSelectInput(d, target.window().into(), 1 | 2 | 4 | 8 | 64 | (1 << 16));
        XSetInputFocus(d, target.window().into(), 2, 0);
        XWarpPointer(d, 0, target.window().into(), 0, 0, 0, 0, 12, 9);
        XSync(d, 1);
    }
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    renderer.present(&pattern(32, 32, 83)).unwrap();
    let canvas = observer.children(target.window());
    assert_eq!(canvas.len(), 1);
    let (mut focus, mut revert) = (0, 0);
    // SAFETY: writable scalars, the original connection/window remain live.
    unsafe { XGetInputFocus(d, &raw mut focus, &raw mut revert) };
    assert_eq!(focus, c_ulong::from(target.window()));
    // SAFETY: XTest is explicitly a fixture, not trusted physical-input evidence.
    // Press and release are both sent before assertions; no held key is leaked.
    unsafe {
        let key = XKeysymToKeycode(d, 0x61);
        assert_ne!(key, 0);
        assert_ne!(XTestFakeButtonEvent(d, 1, 1, 0), 0);
        assert_ne!(XTestFakeButtonEvent(d, 1, 0, 0), 0);
        assert_ne!(XTestFakeKeyEvent(d, key.into(), 1, 0), 0);
        assert_ne!(XTestFakeKeyEvent(d, key.into(), 0, 0), 0);
        XSync(d, 0);
    }
    let mut received = [0; 4];
    let mut count = 0;
    // SAFETY: XPending avoids blocking; XNextEvent fills a correctly aligned
    // native XEvent union. The discriminant is read before each typed member.
    unsafe {
        while XPending(d) != 0 {
            count += 1;
            assert!(count <= 128, "bounded fixture event observation");
            let mut event = Event { padding: [0; 24] };
            XNextEvent(d, &raw mut event);
            match event.kind {
                2..=5 => {
                    let input = event.input;
                    assert_eq!(input.window, u64::from(target.window()));
                    assert_eq!(input.send_event, 0);
                    assert_eq!(input.same_screen, 1);
                    assert_eq!((input.x, input.y), (12, 9));
                    if matches!(input.kind, 4 | 5) {
                        assert_eq!(input.subwindow, u64::from(canvas[0]));
                        assert_eq!(input.detail, 1);
                    }
                    received[usize::try_from(input.kind - 2).unwrap()] += 1;
                }
                15 => {
                    // A subwindow is not an unrelated occluding top-level
                    // window. No native visibility-loss event may be invented.
                    assert_eq!(event.visibility.window, u64::from(target.window()));
                    assert_eq!(event.visibility.state, 0);
                }
                _ => {}
            }
        }
    }
    assert_eq!(received, [1; 4]);
    drop(renderer);
    // SAFETY: dropping the renderer closes only its connection/child, not ours.
    unsafe { XGetInputFocus(d, &raw mut focus, &raw mut revert) };
    assert_eq!(focus, c_ulong::from(target.window()));
}
