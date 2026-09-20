//! Actual isolated X server and independent client used for drawable tests.
use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use fr_core::limits::ProtocolLimits;
use fr_native::{BgraFrame, X11Surface};
use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};

#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessage {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    message_type: c_ulong,
    format: c_int,
    data: [c_long; 5],
}
#[repr(C)]
union Event {
    message: ClientMessage,
    padding: [c_long; 24],
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XSendEvent(
        d: *mut c_void,
        w: c_ulong,
        propagate: c_int,
        mask: c_long,
        event: *mut Event,
    ) -> c_int;
    fn XClearArea(
        d: *mut c_void,
        w: c_ulong,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        exposures: c_int,
    ) -> c_int;
    fn XSync(d: *mut c_void, discard: c_int) -> c_int;
    fn XResizeWindow(d: *mut c_void, w: c_ulong, width: c_uint, height: c_uint) -> c_int;
    fn XUnmapWindow(d: *mut c_void, w: c_ulong) -> c_int;
    fn XMapWindow(d: *mut c_void, w: c_ulong) -> c_int;
    fn XDestroyWindow(d: *mut c_void, w: c_ulong) -> c_int;
}

pub struct Server {
    child: Child,
    pub name: String,
    connection: *mut c_void,
}
impl Server {
    pub fn start() -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "128x128x24",
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
        let name = format!(":{}", number.trim().parse::<u32>().unwrap());
        let c_name = CString::new(name.clone()).unwrap();
        // SAFETY: temporary C string, live Xvfb; connection owned by this fixture.
        let connection = unsafe { XOpenDisplay(c_name.as_ptr()) };
        assert!(!connection.is_null());
        Self {
            child,
            name,
            connection,
        }
    }
    pub fn window(&self) -> X11Surface {
        X11Surface::presenter(Some(&self.name), 64, 64, ProtocolLimits::ABSOLUTE).unwrap()
    }
    pub fn noise(&self, window: u32) {
        // XEvent is a 24-long union; XSendEvent copies the initialized client
        // message fields. This unrelated local event must not keep a waiter busy.
        let mut event = Event {
            message: ClientMessage {
                kind: 33,
                serial: 0,
                send_event: 1,
                display: self.connection,
                window: window.into(),
                message_type: 0,
                format: 32,
                data: [0; 5],
            },
        };
        // SAFETY: live connection and full, correctly aligned XEvent union.
        unsafe {
            assert_ne!(
                XSendEvent(self.connection, window.into(), 0, 1 << 17, &raw mut event),
                0
            );
            XSync(self.connection, 0);
        }
    }
    pub fn clear(&self, window: u32, count: usize) {
        // SAFETY: test-owned window on this server; the client copies scalar args.
        unsafe {
            for _ in 0..count {
                XClearArea(self.connection, window.into(), 0, 0, 0, 0, 1);
            }
            XSync(self.connection, 0);
        }
    }
    pub fn resize_roundtrip(&self, window: u32) {
        // SAFETY: test-owned mapped window, fixed valid dimensions.
        unsafe {
            XResizeWindow(self.connection, window.into(), 32, 32);
            XResizeWindow(self.connection, window.into(), 64, 64);
            XSync(self.connection, 0);
        }
    }
    pub fn unmap_roundtrip(&self, window: u32) {
        // SAFETY: test-owned mapped window; synchronization retains both events.
        unsafe {
            XUnmapWindow(self.connection, window.into());
            XMapWindow(self.connection, window.into());
            XSync(self.connection, 0);
        }
    }
    pub fn destroy(&self, window: u32) {
        // SAFETY: test-owned window, destroyed exactly once by this independent client.
        unsafe {
            XDestroyWindow(self.connection, window.into());
            XSync(self.connection, 0);
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: unique live fixture connection; close before terminating server.
        unsafe {
            XCloseDisplay(self.connection);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
pub fn picture(value: u8) -> BgraFrame {
    let mut pixels = vec![value; 64 * 64 * 4];
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    BgraFrame::new(64, 64, pixels, &ProtocolLimits::ABSOLUTE).unwrap()
}
