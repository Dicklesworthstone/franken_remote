//! Real XCB consent windows and the production `XTest` sink. The window decisions
//! are tested directly, not a fabricated session grant. Non-XTEST Xvfb devices
//! are explicit source-attribution fixtures, never evidence of physical input.
#![cfg(all(target_os = "linux", feature = "linux-input"))]
use fr_core::{
    input::{DesktopPoint, PointerButton},
    input_submission::{InputSink, Operation, Submission},
};
use fr_native::input::X11Pointer;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    ptr::NonNull,
    thread,
    time::{Duration, Instant},
};

#[link(name = "frindicator", kind = "static")]
unsafe extern "C" {
    fn fr_approval_open(display: *const c_char, role: u32, window: *mut u32) -> *mut c_void;
    fn fr_indicator_open(display: *const c_char, window: *mut u32) -> *mut c_void;
    fn fr_indicator_open_control(display: *const c_char, window: *mut u32) -> *mut c_void;
    fn fr_indicator_close(handle: *mut c_void);
    fn fr_indicator_next(handle: *mut c_void, kind: *mut u32) -> c_int;
    fn fr_indicator_draw(handle: *mut c_void) -> c_int;
}
const MAPPED: u32 = 2;
const STOP: u32 = 5;
const ALLOW: u32 = 6;
struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb required");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Panel {
    native: NonNull<c_void>,
    window: u32,
}
impl Panel {
    fn open(display: &str, mode: u32) -> Self {
        let name = CString::new(display).unwrap();
        let mut window = 0;
        // SAFETY: live string/output pointer for this call, no retained Rust
        // pointers. The returned allocation stays on this thread until Drop.
        let native = unsafe {
            match mode {
                0 => fr_indicator_open(name.as_ptr(), &raw mut window),
                1 | 2 => fr_approval_open(name.as_ptr(), mode - 1, &raw mut window),
                3 => fr_indicator_open_control(name.as_ptr(), &raw mut window),
                _ => unreachable!(),
            }
        };
        let mut panel = Self {
            native: NonNull::new(native).expect("XI2 consent window"),
            window,
        };
        // SAFETY: exclusively owned live native window.
        assert_eq!(unsafe { fr_indicator_draw(panel.native.as_ptr()) }, 1);
        assert!(panel.events().contains(&MAPPED));
        panel
    }
    fn events(&mut self) -> Vec<u32> {
        let until = Instant::now() + Duration::from_millis(100);
        let mut events = Vec::new();
        loop {
            let mut kind = 0;
            // SAFETY: original live connection, fixed writable output, no alias.
            let result = unsafe { fr_indicator_next(self.native.as_ptr(), &raw mut kind) };
            assert!(result >= 0, "native window failure");
            if result == 1 {
                events.push(kind);
            }
            if Instant::now() >= until {
                return events;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn peer(&mut self, display: &str, operation: &str) -> Vec<u32> {
        assert!(
            Command::new("python3")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/sharing_indicator/peer.py"
                ))
                .arg(display)
                .arg(self.window.to_string())
                .arg(operation)
                .status()
                .unwrap()
                .success()
        );
        self.events()
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        // SAFETY: free exactly the allocation returned by open, on its owner.
        unsafe {
            fr_indicator_close(self.native.as_ptr());
        }
    }
}
fn submit(sink: &mut X11Pointer, operation: Operation) {
    sink.prepare(operation).unwrap();
    assert_eq!(sink.submit(operation), Submission::Submitted);
}
fn injected_click(display: &str) {
    // The same production native sink used by the per-lease input executor.
    // No network/authority is invented here; this isolates the UI boundary.
    let mut sink = X11Pointer::open(display).unwrap();
    submit(
        &mut sink,
        Operation::Absolute(DesktopPoint { x: 352, y: 97 }),
    );
    for pressed in [true, false] {
        submit(
            &mut sink,
            Operation::Button {
                button: PointerButton::Primary,
                pressed,
            },
        );
    }
}
#[test]
fn production_injected_click_cannot_approve_either_role() {
    let server = Server::start();
    for role in [1, 2] {
        let mut panel = Panel::open(&server.display, role);
        injected_click(&server.display);
        let events = panel.events();
        assert!(
            !events.contains(&ALLOW),
            "production XTest sink approved role {role}: {events:?}"
        );
        assert!(
            !events.contains(&STOP),
            "injected input operated consent UI"
        );
        assert!(panel.peer(&server.display, "device-allow").contains(&ALLOW));
    }
}
#[test]
fn send_event_unpaired_release_and_drag_out_never_approve() {
    let server = Server::start();
    let mut panel = Panel::open(&server.display, 1);
    for operation in [
        "synthetic-allow",
        "release-allow",
        "device-release-allow",
        "device-drag-out",
    ] {
        let events = panel.peer(&server.display, operation);
        assert!(!events.contains(&ALLOW), "{operation}: {events:?}");
    }
    assert!(panel.peer(&server.display, "device-allow").contains(&ALLOW));
}
#[test]
fn synthetic_clicks_and_keys_cannot_operate_either_indicator() {
    let server = Server::start();
    for mode in [0, 3] {
        let mut panel = Panel::open(&server.display, mode);
        for operation in ["click", "key"] {
            let events = panel.peer(&server.display, operation);
            assert!(
                !events.contains(&STOP),
                "mode={mode} {operation}: {events:?}"
            );
        }
        assert!(panel.peer(&server.display, "device-click").contains(&STOP));
    }
}
#[test]
fn device_keyboard_is_negative_authority_only_for_every_mode() {
    let server = Server::start();
    for mode in 0..4 {
        let mut panel = Panel::open(&server.display, mode);
        let events = panel.peer(&server.display, "device-key");
        assert!(events.contains(&STOP));
        assert!(!events.contains(&ALLOW));
    }
}

#[test]
fn stale_or_interrupted_device_click_cannot_leave_reusable_consent() {
    let server = Server::start();
    let mut panel = Panel::open(&server.display, 1);
    assert!(
        !panel
            .peer(&server.display, "device-press-allow")
            .contains(&ALLOW)
    );
    thread::sleep(Duration::from_millis(2_100));
    assert!(
        !panel
            .peer(&server.display, "device-release-allow")
            .contains(&ALLOW)
    );
    assert!(
        !panel
            .peer(&server.display, "device-press-allow")
            .contains(&ALLOW)
    );
    assert!(
        !panel
            .peer(&server.display, "synthetic-allow")
            .contains(&ALLOW)
    );
    assert!(
        !panel
            .peer(&server.display, "device-release-allow")
            .contains(&ALLOW)
    );
    assert!(panel.peer(&server.display, "device-allow").contains(&ALLOW));
}
