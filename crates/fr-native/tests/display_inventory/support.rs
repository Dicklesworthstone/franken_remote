//! Shared actual Xvfb/RandR fixture; no simulated catalog or capture.
use fr_core::limits::ProtocolLimits;
use fr_native::displays::X11Inventory;
use std::{
    ffi::{CString, c_char, c_int, c_ulong, c_void},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

#[repr(C)]
struct Monitor {
    name: c_ulong,
    primary: c_int,
    automatic: c_int,
    noutput: c_int,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    mwidth: c_int,
    mheight: c_int,
    outputs: *mut c_ulong,
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XInternAtom(display: *mut c_void, name: *const c_char, only: c_int) -> c_ulong;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XCreateGC(
        display: *mut c_void,
        root: c_ulong,
        mask: c_ulong,
        values: *mut c_void,
    ) -> *mut c_void;
    fn XFreeGC(display: *mut c_void, gc: *mut c_void) -> c_int;
    fn XSetForeground(display: *mut c_void, gc: *mut c_void, pixel: c_ulong) -> c_int;
    fn XFillRectangle(
        display: *mut c_void,
        drawable: c_ulong,
        gc: *mut c_void,
        x: c_int,
        y: c_int,
        width: u32,
        height: u32,
    ) -> c_int;
}
#[link(name = "libXrandr.so.2", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XRRSetMonitor(display: *mut c_void, root: c_ulong, monitor: *mut Monitor);
    fn XRRDeleteMonitor(display: *mut c_void, root: c_ulong, atom: c_ulong);
}
pub(super) struct Screen {
    child: Child,
    pub(super) name: String,
    display: *mut c_void,
    root: c_ulong,
}
impl Screen {
    pub(super) fn start() -> Self {
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
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut number)
            .unwrap();
        let name = format!(":{}", number.trim().parse::<u32>().unwrap());
        let c_name = CString::new(name.as_str()).unwrap();
        let display = unsafe { XOpenDisplay(c_name.as_ptr()) };
        assert!(!display.is_null());
        let root = unsafe { XDefaultRootWindow(display) };
        Self {
            child,
            name,
            display,
            root,
        }
    }
    pub(super) fn atom(&self, name: &str) -> c_ulong {
        let n = CString::new(name).unwrap();
        unsafe { XInternAtom(self.display, n.as_ptr(), 0) }
    }
    pub(super) fn monitor(&self, name: &str, x: i32, y: i32, width: i32, height: i32) {
        let mut monitor = Monitor {
            name: self.atom(name),
            primary: 0,
            automatic: 0,
            noutput: 0,
            x,
            y,
            width,
            height,
            mwidth: 100,
            mheight: 100,
            outputs: core::ptr::null_mut(),
        };
        unsafe {
            XRRSetMonitor(self.display, self.root, &raw mut monitor);
            XSync(self.display, 0);
        }
    }
    pub(super) fn remove(&self, name: &str) {
        unsafe {
            XRRDeleteMonitor(self.display, self.root, self.atom(name));
            XSync(self.display, 0);
        }
    }
    pub(super) fn paint(&self, x: i32, width: u32, color: c_ulong) {
        unsafe {
            let gc = XCreateGC(self.display, self.root, 0, core::ptr::null_mut());
            assert!(!gc.is_null());
            XSetForeground(self.display, gc, color);
            XFillRectangle(self.display, self.root, gc, x, 0, width, 480);
            XFreeGC(self.display, gc);
            XSync(self.display, 0);
        }
    }
    pub(super) fn inventory(&self) -> X11Inventory {
        X11Inventory::open(&self.name, ProtocolLimits::ABSOLUTE).unwrap()
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
