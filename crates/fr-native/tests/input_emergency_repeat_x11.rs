//! Real X11 crash cleanup on a private Xvfb. A second connection observes
//! per-key repeat settings; neither the injector nor its journal supplies the
//! expected result. Each fault runs in a child because the handlers exit.
#![cfg(all(target_os = "linux", feature = "linux-input"))]
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use fr_core::{
    input::{KeyTransition, PhysicalKey},
    input_submission::{InputSink, Operation, Submission},
};
use fr_native::input::{X11Pointer, emergency};
use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
};

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
#[repr(C)]
struct KeyboardControl {
    key_click_percent: c_int,
    bell_percent: c_int,
    bell_pitch: c_int,
    bell_duration: c_int,
    led: c_int,
    led_mode: c_int,
    key: c_int,
    auto_repeat_mode: c_int,
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XKeysymToKeycode(display: *mut c_void, sym: c_ulong) -> u8;
    fn XQueryKeymap(display: *mut c_void, keys: *mut u8) -> c_int;
    fn XGetKeyboardControl(display: *mut c_void, state: *mut KeyboardState) -> c_int;
    fn XChangeKeyboardControl(
        display: *mut c_void,
        mask: c_ulong,
        control: *mut KeyboardControl,
    ) -> c_int;
    fn XMapWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XConnectionNumber(display: *mut c_void) -> c_int;
    fn close(fd: c_int) -> c_int;
}

struct Server(Child, String);
impl Server {
    fn start() -> Self {
        fr_native::xlib::initialize_threads().unwrap();
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
            .expect("install Xvfb for native input tests");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self(child, format!(":{}", number.trim().parse::<u16>().unwrap()))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Observer(*mut c_void);
impl Observer {
    fn open(display: &str) -> Self {
        let name = CString::new(display).unwrap();
        // SAFETY: the name lives through the call; this owner closes the result.
        let display = unsafe { XOpenDisplay(name.as_ptr()) };
        assert!(!display.is_null());
        Self(display)
    }
    fn code(&self, symbol: c_ulong) -> u8 {
        // SAFETY: live connection, scalar X keysym.
        let code = unsafe { XKeysymToKeycode(self.0, symbol) };
        assert!(code >= 8);
        code
    }
    fn down(&self, code: u8) -> bool {
        let mut keys = [0; 32];
        // SAFETY: live connection and the complete writable XQueryKeymap buffer.
        assert_ne!(unsafe { XQueryKeymap(self.0, keys.as_mut_ptr()) }, 0);
        keys[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn repeat(&self, code: u8) -> bool {
        let mut state = KeyboardState {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led_mask: 0,
            global_auto_repeat: 0,
            auto_repeats: [0; 32],
        };
        // SAFETY: live connection and Xlib.h-layout writable output.
        assert_ne!(unsafe { XGetKeyboardControl(self.0, &raw mut state) }, 0);
        state.auto_repeats[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn set_repeat(&self, code: u8, enabled: bool) {
        let mut control = KeyboardControl {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led: 0,
            led_mode: 0,
            key: c_int::from(code),
            auto_repeat_mode: c_int::from(enabled),
        };
        // SAFETY: test-owned display and valid keycode; KBKey | KBAutoRepeatMode
        // changes only this key's repeat bit, never the global repeat setting.
        unsafe {
            XChangeKeyboardControl(self.0, (1 << 6) | (1 << 7), &raw mut control);
            XSync(self.0, 0);
        }
        assert_eq!(self.repeat(code), enabled);
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: this owner closes its live connection exactly once.
        unsafe { XCloseDisplay(self.0) };
    }
}

const CHILD: &str = "FR_INPUT_REPEAT_VICTIM";
const TEST: &str = "emergency_restores_repeat_and_preserves_completed_cleanup";
const ARMED: &[u8] = b"FR_REPEAT_FIXTURE_ARMED\n";
fn key(transition: KeyTransition) -> Operation {
    Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn victim(display: &str, phase: &str, fault: &str) -> ! {
    let mut native = X11Pointer::open(display).unwrap();
    emergency::install(display).unwrap();
    let observer = Observer::open(display);
    let a = observer.code(0x61);
    let original = observer.repeat(a);
    let press = key(KeyTransition::Press);
    native.prepare(press).unwrap();
    assert!(!observer.repeat(a), "preflight must suppress server repeat");
    match phase {
        // Intentionally do not record native_held: a failure may occur inside
        // preparation, before the executor publishes its next held snapshot.
        "prepared" => assert!(!observer.down(a)),
        "cancelled" => {
            native.cancel_prepared();
            assert_eq!(observer.repeat(a), original);
            // A later local change must survive emergency cleanup: the old
            // remote preparation no longer owns this keyboard setting.
            observer.set_repeat(a, !original);
        }
        "pressed" | "released" => {
            assert_eq!(
                native.submit_checked(press, |held| {
                    emergency::record(held);
                    Ok::<(), ()>(())
                }),
                Ok(Submission::Submitted)
            );
            emergency::record(native.native_held());
            assert!(observer.down(a));
            if phase == "released" {
                let release = key(KeyTransition::Release);
                native.prepare(release).unwrap();
                assert_eq!(native.submit(release), Submission::Submitted);
                emergency::record(native.native_held());
                assert!(!observer.down(a));
                assert_eq!(observer.repeat(a), original);
                observer.set_repeat(a, !original);
            }
        }
        other => panic!("unknown victim phase {other}"),
    }
    // Earlier assertion failures also enter the installed panic hook. Require
    // proof that setup completed so those cannot masquerade as a passing fault.
    let mut signal = std::io::stdout().lock();
    signal.write_all(ARMED).unwrap();
    signal.flush().unwrap();
    drop(signal);
    match fault {
        "panic" => panic!("intentional input owner failure"),
        "xlib-error" => {
            // SAFETY: deliberately invalid request on this victim's connection.
            unsafe {
                XMapWindow(observer.0, 0x7fff_fff0);
                XSync(observer.0, 0);
            }
        }
        "xlib-io" => {
            // SAFETY: close the victim socket behind Xlib to force its fatal
            // I/O handler. That handler exits; no owner can reuse this handle.
            unsafe {
                close(XConnectionNumber(observer.0));
                XSync(observer.0, 0);
            }
        }
        other => panic!("unknown victim fault {other}"),
    }
    std::process::abort()
}

#[test]
fn emergency_restores_repeat_and_preserves_completed_cleanup() {
    if let Ok(value) = std::env::var(CHILD) {
        let mut parts = value.split('@');
        let phase = parts.next().unwrap();
        let fault = parts.next().unwrap();
        let display = parts.next().unwrap();
        assert!(parts.next().is_none());
        victim(display, phase, fault);
    }
    let server = Server::start();
    let observer = Observer::open(&server.1);
    let a = observer.code(0x61);
    let untouched = observer.code(0x62);
    for original in [true, false] {
        for phase in ["prepared", "pressed", "cancelled", "released"] {
            for (fault, exit) in [
                ("panic", emergency::EXIT_PANIC),
                ("xlib-error", emergency::EXIT_XLIB_ERROR),
                ("xlib-io", emergency::EXIT_XLIB_IO),
            ] {
                observer.set_repeat(a, original);
                observer.set_repeat(untouched, !original);
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env(CHILD, format!("{phase}@{fault}@{}", server.1))
                    .output()
                    .unwrap();
                let context = format!(
                    "{phase}, {fault}, original repeat={original}\nfixture stdout: {}\nfixture stderr: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                assert!(
                    output
                        .stdout
                        .windows(ARMED.len())
                        .any(|bytes| bytes == ARMED),
                    "fixture setup did not complete: {context}"
                );
                assert_eq!(output.status.code(), Some(exit), "{context}");
                assert!(!observer.down(a), "key left held: {context}");
                let expected = if matches!(phase, "cancelled" | "released") {
                    !original
                } else {
                    original
                };
                assert_eq!(
                    observer.repeat(a),
                    expected,
                    "repeat not restored: {context}"
                );
                assert_eq!(
                    observer.repeat(untouched),
                    !original,
                    "unowned key: {context}"
                );
            }
        }
    }
}
