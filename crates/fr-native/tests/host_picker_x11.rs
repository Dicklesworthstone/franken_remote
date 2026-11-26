#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual local X11 picker lifecycle. This is not remote authentication proof.

use fr_native::host_picker::{Choices, Control, HostPicker, PathHint, Row, Status};
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
    fn XGetKeyboardMapping(
        display: *mut c_void,
        first: u8,
        count: c_int,
        per_code: *mut c_int,
    ) -> *mut c_ulong;
    fn XChangeKeyboardMapping(
        display: *mut c_void,
        first: c_int,
        per_code: c_int,
        symbols: *const c_ulong,
        count: c_int,
    ) -> c_int;
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
        let screen = Self::unprimed();
        // Xvfb's first XTest use creates its device and changes the keymap.
        // Establish that device before the production picker pins its map.
        screen.key(0xffe1, true);
        screen.key(0xffe1, false);
        screen
    }
    // A display whose first XTest key still switches the core keyboard to
    // the XTest device, which XKB announces as a new keyboard, as it does
    // when input moves to a second physical keyboard.
    fn unprimed() -> Self {
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
        Self {
            child,
            name,
            display,
        }
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
    // Writes one key's keysyms back unchanged. The server still announces a
    // keymap change (core MappingNotify and XKB MapNotify).
    fn replace_keymap(&self, symbol: c_ulong) {
        // SAFETY: this fixture owns the live display on this thread. Xlib
        // allocates per_code keysyms for the one keycode; they are passed
        // back for that same keycode and freed once.
        unsafe {
            let key = XKeysymToKeycode(self.display.as_ptr(), symbol);
            assert_ne!(key, 0);
            let mut per_code = 0;
            let symbols = XGetKeyboardMapping(self.display.as_ptr(), key, 1, &raw mut per_code);
            assert!(!symbols.is_null() && per_code > 0);
            XChangeKeyboardMapping(
                self.display.as_ptr(),
                c_int::from(key),
                per_code,
                symbols,
                1,
            );
            XFree(symbols.cast());
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
fn chosen(picker: &mut HostPicker) -> usize {
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    loop {
        if let Some(ordinal) = picker.poll().unwrap() {
            return ordinal;
        }
        assert!(Instant::now() < deadline, "fresh Enter did not choose");
        thread::sleep(Duration::from_millis(2));
    }
}
fn enter(screen: &Screen) {
    screen.key(0xff0d, true);
    thread::sleep(Duration::from_millis(15));
    screen.key(0xff0d, false);
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
fn actual_choice_keys_change_only_the_confirmed_choices() {
    let screen = Screen::start();
    let initial = Choices {
        control: true,
        clipboard: true,
        choose_display: false,
        playback_audio: false,
        send_files: false,
        choose_files: false,
        receive_available: false,
        receive_files: false,
        microphone_available: false,
        microphone_controls: false,
    };
    let mut picker = HostPicker::start_with(
        &screen.name,
        &rows(12),
        Some(initial),
        Some("host-3: The host did not complete the connection in time."),
    )
    .unwrap();
    let window = mapped(&mut picker);
    screen.focus(window);
    // F3/F4/F5 change clipboard/display/audio; F2 twice leaves control on.
    for symbol in [0xffc0, 0xffc1, 0xffc2, 0xffbf, 0xffbf] {
        screen.key(symbol, true);
        screen.key(symbol, false);
        thread::sleep(Duration::from_millis(10));
    }
    screen.key(0xff54, true); // Down: the second row.
    screen.key(0xff54, false);
    thread::sleep(Duration::from_millis(25));
    // No toggle confirms a row.
    assert_eq!(picker.poll().unwrap(), None);
    assert_eq!(picker.choices(), None);
    screen.key(0xff0d, true);
    thread::sleep(Duration::from_millis(15));
    screen.key(0xff0d, false);
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    let ordinal = loop {
        if let Some(ordinal) = picker.poll().unwrap() {
            break ordinal;
        }
        assert!(Instant::now() < deadline, "fresh Enter did not choose");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(ordinal, 1);
    assert_eq!(
        picker.choices(),
        Some(Choices {
            control: true,
            clipboard: false,
            choose_display: true,
            playback_audio: cfg!(feature = "linux-audio"),
            send_files: false,
            choose_files: false,
            receive_available: false,
            receive_files: false,
            microphone_available: false,
            microphone_controls: false,
        })
    );
}

#[test]
fn actual_audio_choice_works_in_view_only_and_respects_build_availability() {
    let screen = Screen::start();
    for initial_audio in [false, true] {
        let mut picker = HostPicker::start_with(
            &screen.name,
            &rows(3),
            Some(Choices {
                control: false,
                clipboard: true,
                choose_display: false,
                playback_audio: initial_audio,
                send_files: false,
                choose_files: false,
                receive_available: false,
                receive_files: false,
                microphone_available: false,
                microphone_controls: false,
            }),
            None,
        )
        .unwrap();
        let window = mapped(&mut picker);
        screen.focus(window);
        screen.key(0xffc2, true); // F5 is independent of control.
        screen.key(0xffc2, false);
        thread::sleep(Duration::from_millis(25));
        assert_eq!(picker.poll().unwrap(), None);
        assert_eq!(picker.choices(), None);
        enter(&screen);
        assert_eq!(chosen(&mut picker), 0);
        assert_eq!(
            picker.choices(),
            Some(Choices {
                control: false,
                clipboard: true,
                choose_display: false,
                playback_audio: cfg!(feature = "linux-audio") && !initial_audio,
                send_files: false,
                choose_files: false,
                receive_available: false,
                receive_files: false,
                microphone_available: false,
                microphone_controls: false,
            })
        );
    }
}

#[test]
fn actual_file_sending_requires_a_fresh_control_choice() {
    let screen = Screen::start();
    for (control, keys, expected_drop, expected_pick) in [
        (true, &[][..], false, false),
        (false, &[0xffc3][..], false, false),
        (true, &[0xffc3][..], true, false),
        (true, &[0xffc3, 0xffbf, 0xffbf][..], false, false),
        (true, &[0xffc3, 0xffbf, 0xffbf, 0xffc3][..], true, false),
        (true, &[0xffc6][..], false, true),
        (false, &[0xffc6][..], false, false),
        (true, &[0xffc6, 0xffbf, 0xffbf][..], false, false),
        (true, &[0xffc3, 0xffc6][..], true, true),
        (true, &[0xffc6, 0xffc3][..], true, true),
    ] {
        let mut picker = HostPicker::start_with(
            &screen.name,
            &rows(3),
            Some(Choices {
                control,
                ..Choices::default()
            }),
            None,
        )
        .unwrap();
        let window = mapped(&mut picker);
        screen.focus(window);
        for &key in keys {
            screen.key(key, true);
            screen.key(key, false);
            thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(picker.poll().unwrap(), None);
        assert_eq!(picker.choices(), None);
        enter(&screen);
        assert_eq!(chosen(&mut picker), 0);
        let choices = picker.choices().unwrap();
        assert_eq!(choices.control, control);
        assert_eq!(choices.send_files, expected_drop);
        assert_eq!(choices.choose_files, expected_pick);
    }
}

#[test]
fn actual_file_receiving_requires_the_original_destination_and_a_fresh_control_choice() {
    let screen = Screen::start();
    for (control, available, keys, expected_control, expected_receive) in [
        (true, true, &[][..], true, false),
        (true, false, &[0xffc4][..], true, false),
        (false, true, &[0xffc4][..], false, false),
        (true, true, &[0xffc4][..], true, true),
        (false, true, &[0xffbf, 0xffc4][..], true, true),
        (true, true, &[0xffc4, 0xffbf, 0xffbf][..], true, false),
        (
            true,
            true,
            &[0xffc4, 0xffbf, 0xffbf, 0xffc4][..],
            true,
            true,
        ),
    ] {
        let mut picker = HostPicker::start_with(
            &screen.name,
            &rows(3),
            Some(Choices {
                control,
                receive_available: available,
                // Reusing a previous result cannot preselect receiving in a
                // newly opened native picker.
                receive_files: true,
                ..Choices::default()
            }),
            None,
        )
        .unwrap();
        let window = mapped(&mut picker);
        screen.focus(window);
        for &key in keys {
            screen.key(key, true);
            screen.key(key, false);
            thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(picker.poll().unwrap(), None);
        assert_eq!(picker.choices(), None);
        enter(&screen);
        assert_eq!(chosen(&mut picker), 0);
        let choices = picker.choices().unwrap();
        assert_eq!(choices.control, expected_control);
        assert_eq!(choices.receive_available, available);
        assert_eq!(choices.receive_files, expected_receive);
        assert!(!choices.send_files);
    }
}

#[test]
fn actual_picker_without_choices_reports_none() {
    let screen = Screen::start();
    let mut picker = HostPicker::start(&screen.name, &rows(3)).unwrap();
    let window = mapped(&mut picker);
    screen.focus(window);
    screen.key(0xffc0, true); // F3 has nothing to toggle here.
    screen.key(0xffc0, false);
    thread::sleep(Duration::from_millis(25));
    screen.key(0xff0d, true);
    thread::sleep(Duration::from_millis(15));
    screen.key(0xff0d, false);
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    while picker.poll().unwrap().is_none() {
        assert!(Instant::now() < deadline, "fresh Enter did not choose");
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(picker.choices(), None);
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

#[test]
fn actual_keymap_change_drops_the_enter_it_interrupts_and_keeps_the_picker() {
    let screen = Screen::start();
    let mut picker = HostPicker::start(&screen.name, &rows(12)).unwrap();
    let window = mapped(&mut picker);
    screen.focus(window);
    screen.key(0xff54, true); // Down: the second row.
    screen.key(0xff54, false);
    thread::sleep(Duration::from_millis(25));
    screen.key(0xff0d, true);
    screen.replace_keymap(0xffe1);
    thread::sleep(Duration::from_millis(15));
    screen.key(0xff0d, false);
    // The Enter pressed through the old map never confirms.
    let quiet = Instant::now()
        .checked_add(Duration::from_millis(300))
        .unwrap();
    while Instant::now() < quiet {
        assert_eq!(picker.poll().unwrap(), None);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(picker.status(), Status::Mapped);
    screen.key(0xff54, true); // Down through the new map: the third row.
    screen.key(0xff54, false);
    thread::sleep(Duration::from_millis(25));
    enter(&screen);
    assert_eq!(chosen(&mut picker), 2);
}

#[test]
fn actual_first_key_from_a_new_keyboard_device_still_navigates() {
    let screen = Screen::unprimed();
    let mut picker = HostPicker::start(&screen.name, &rows(12)).unwrap();
    let window = mapped(&mut picker);
    screen.focus(window);
    screen.key(0xff54, true); // Down, from the device XKB announces as new.
    screen.key(0xff54, false);
    thread::sleep(Duration::from_millis(25));
    enter(&screen);
    assert_eq!(chosen(&mut picker), 1);
}
