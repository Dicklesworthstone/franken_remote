#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual local X11 picker lifecycle. This is not remote authentication proof.

use fr_native::host_picker::{Control, HostPicker, PathHint, Row, Status};
use std::{
    ffi::{CString, c_char, c_int, c_uint, c_ulong, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    ptr::NonNull,
    thread,
    time::{Duration, Instant},
};

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XKeysymToKeycode(display: *mut c_void, symbol: c_ulong) -> u8;
    fn XSetInputFocus(display: *mut c_void, window: c_ulong, revert: c_int, time: c_ulong)
    -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XQueryTree(
        display: *mut c_void,
        window: c_ulong,
        root: *mut c_ulong,
        parent: *mut c_ulong,
        children: *mut *mut c_ulong,
        count: *mut c_uint,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
}
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestFakeKeyEvent(display: *mut c_void, key: c_uint, down: c_int, delay: c_ulong) -> c_int;
}

struct Screen {
    child: Child,
    name: String,
    display: NonNull<c_void>,
}
impl Screen {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1024x768x24",
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
        let name = format!(":{}", number.trim().parse::<u16>().unwrap());
        let c_name = CString::new(name.clone()).unwrap();
        // SAFETY: the name is a live NUL-terminated local display string.
        let display = NonNull::new(unsafe { XOpenDisplay(c_name.as_ptr()) }).unwrap();
        let screen = Self {
            child,
            name,
            display,
        };
        // Xvfb's first XTest use creates its device and changes the keymap.
        // Establish that device before the production picker pins its map.
        screen.key(0xffe1, true);
        screen.key(0xffe1, false);
        screen
    }
    fn key(&self, symbol: c_ulong, down: bool) {
        // SAFETY: this fixture owns the live display on this thread; scalar
        // keycode and event arguments follow Xlib/XTest's ABI.
        unsafe {
            let key = XKeysymToKeycode(self.display.as_ptr(), symbol);
            assert_ne!(key, 0);
            assert_ne!(
                XTestFakeKeyEvent(self.display.as_ptr(), u32::from(key), i32::from(down), 0),
                0
            );
            XSync(self.display.as_ptr(), 0);
        }
    }
    fn focus(&self, window: u32) {
        // SAFETY: window is the original mapped production picker on this
        // display. CurrentTime is only for the local test's focus operation.
        unsafe {
            XSetInputFocus(self.display.as_ptr(), c_ulong::from(window), 0, 0);
            XSync(self.display.as_ptr(), 0);
        }
        thread::sleep(Duration::from_millis(50));
    }
    fn contains(&self, window: u32) -> bool {
        let (mut root, mut parent, mut children, mut count) = (0, 0, std::ptr::null_mut(), 0);
        // SAFETY: all output pointers are valid; XQueryTree allocates count
        // native window IDs, which are borrowed only until the matching XFree.
        unsafe {
            assert_ne!(
                XQueryTree(
                    self.display.as_ptr(),
                    XDefaultRootWindow(self.display.as_ptr()),
                    &raw mut root,
                    &raw mut parent,
                    &raw mut children,
                    &raw mut count
                ),
                0
            );
            let found = count != 0
                && std::slice::from_raw_parts(children, count as usize)
                    .contains(&c_ulong::from(window));
            if !children.is_null() {
                XFree(children.cast());
            }
            found
        }
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        // SAFETY: the fixture exclusively owns this Xlib connection.
        unsafe { XCloseDisplay(self.display.as_ptr()) };
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rows(count: usize) -> Vec<Row> {
    (0..count)
        .map(|index| {
            Row::new(
                &format!("host-{index}.tail.example"),
                &format!("node-{index}"),
                PathHint::Unknown,
            )
            .unwrap()
        })
        .collect()
}
fn mapped(picker: &mut HostPicker) -> u32 {
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    loop {
        assert_eq!(picker.poll().unwrap(), None);
        if picker.status() == Status::Mapped {
            return picker.window().unwrap();
        }
        assert!(Instant::now() < deadline, "picker did not map");
        thread::sleep(Duration::from_millis(2));
    }
}
fn complete(control: &Control) {
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    while !control.is_complete() {
        assert!(Instant::now() < deadline, "native picker did not join");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn actual_1024_row_selection_returns_original_ordinal_only_after_native_join() {
    let screen = Screen::start();
    let mut picker = HostPicker::start(&screen.name, &rows(1024)).unwrap();
    let window = mapped(&mut picker);
    assert!(screen.contains(window));
    screen.focus(window);
    screen.key(0xff57, true); // End, beyond every initial visible page.
    screen.key(0xff57, false);
    thread::sleep(Duration::from_millis(25));
    screen.key(0xff0d, true);
    thread::sleep(Duration::from_millis(15));
    assert_eq!(picker.poll().unwrap(), None);
    screen.key(0xff0d, false);
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    let ordinal = loop {
        if let Some(ordinal) = picker.poll().unwrap() {
            break ordinal;
        }
        assert!(Instant::now() < deadline, "fresh Enter did not choose");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(ordinal, 1023);
    assert!(picker.is_complete());
    assert_eq!(picker.window(), None);
    assert_eq!(picker.status(), Status::Consumed);
    assert_eq!(picker.poll().unwrap(), None);
    while screen.contains(window) {
        assert!(
            Instant::now() < deadline,
            "original native window was retained"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn actual_escape_is_terminal_cancelled_after_join_and_never_a_choice() {
    let screen = Screen::start();
    let mut picker = HostPicker::start(&screen.name, &rows(12)).unwrap();
    let window = mapped(&mut picker);
    screen.focus(window);
    screen.key(0xff1b, true);
    screen.key(0xff1b, false);
    complete(&picker.control());
    assert_eq!(picker.status(), Status::Cancelled);
    assert_eq!(picker.poll().unwrap(), None);
    assert_eq!(picker.window(), None);
}

#[test]
fn dropping_actual_picker_requests_local_cleanup_without_releasing_selection() {
    let screen = Screen::start();
    let mut picker = HostPicker::start(&screen.name, &rows(12)).unwrap();
    mapped(&mut picker);
    let control = picker.control();
    drop(picker);
    complete(&control);
    assert_eq!(control.status(), Status::Cancelled);
    assert_eq!(control.window(), None);
}
