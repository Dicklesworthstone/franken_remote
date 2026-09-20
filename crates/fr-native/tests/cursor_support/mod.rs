//! Real X server and cursor creation; no fake native cursor provider.
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};
#[repr(C)]
pub struct Image {
    version: u32,
    size: u32,
    width: u32,
    height: u32,
    xhot: u32,
    yhot: u32,
    delay: u32,
    pixels: *mut u32,
}
#[link(name = "libXcursor.so.1", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XcursorImageCreate(width: c_int, height: c_int) -> *mut Image;
    fn XcursorImageDestroy(image: *mut Image);
    fn XcursorImageLoadCursor(display: *mut c_void, image: *const Image) -> c_ulong;
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XDefineCursor(display: *mut c_void, window: c_ulong, cursor: c_ulong) -> c_int;
    fn XFreeCursor(display: *mut c_void, cursor: c_ulong) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XWarpPointer(
        display: *mut c_void,
        source: c_ulong,
        dest: c_ulong,
        sx: c_int,
        sy: c_int,
        width: c_uint,
        height: c_uint,
        x: c_int,
        y: c_int,
    ) -> c_int;
}
#[link(name = "libXfixes.so.3", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XFixesHideCursor(display: *mut c_void, window: c_ulong);
}
pub struct Server {
    child: Option<Child>,
    pub name: String,
    pub display: *mut c_void,
    root: c_ulong,
}
impl Server {
    pub fn start(fixes: bool) -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let mut command = Command::new("Xvfb");
        command.args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "640x480x24",
            "-nolisten",
            "tcp",
            "-noreset",
        ]);
        if !fixes {
            command.args(["-extension", "XFIXES"]);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        let name = format!(":{}", number.trim().parse::<u32>().unwrap());
        // The absent-extension fixture needs only the production connection.
        // This Xvfb build crashes in CloseDownClient with XFIXES disabled; an
        // unnecessary second observer would turn that server teardown defect
        // into an unrelated fatal Xlib I/O callback in the test process.
        let mut server = if fixes {
            Self::connect(&name)
        } else {
            Self {
                child: None,
                name,
                display: core::ptr::null_mut(),
                root: 0,
            }
        };
        server.child = Some(child);
        server
    }
    pub fn connect(name: &str) -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let name_c = CString::new(name).unwrap();
        // SAFETY: fixture-owned connection and live server; no pointer escapes.
        let display = unsafe { XOpenDisplay(name_c.as_ptr()) };
        assert!(!display.is_null());
        // SAFETY: valid open connection.
        let root = unsafe { XDefaultRootWindow(display) };
        Self {
            child: None,
            name: name.to_owned(),
            display,
            root,
        }
    }
    pub fn cursor(&self, width: i32, height: i32, hotspot: (u32, u32), argb: u32) {
        // SAFETY: fixed bounded fixture geometry; the library owns image+pixels.
        unsafe {
            let image = XcursorImageCreate(width, height);
            assert!(!image.is_null());
            (*image).xhot = hotspot.0;
            (*image).yhot = hotspot.1;
            core::slice::from_raw_parts_mut(
                (*image).pixels,
                usize::try_from(width.checked_mul(height).unwrap()).unwrap(),
            )
            .fill(argb);
            let cursor = XcursorImageLoadCursor(self.display, image);
            XcursorImageDestroy(image);
            assert_ne!(cursor, 0);
            XDefineCursor(self.display, self.root, cursor);
            XFreeCursor(self.display, cursor);
            XSync(self.display, 0);
        }
    }
    pub fn warp(&self, x: i32, y: i32) {
        // SAFETY: independent test input on the fixture root, not production authority.
        unsafe {
            XWarpPointer(self.display, 0, self.root, 0, 0, 0, 0, x, y);
            XSync(self.display, 0);
        }
    }
    pub fn hide(&self) {
        // SAFETY: fixture's own hide reference, removed when its connection closes.
        unsafe {
            XFixesHideCursor(self.display, self.root);
            XSync(self.display, 0);
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: close our unique client before terminating the owned server.
        if !self.display.is_null() {
            unsafe {
                XCloseDisplay(self.display);
            }
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
