#![cfg(all(target_os = "linux", feature = "linux-displays"))]
//! Actual `RandR` monitor discovery/changes and selected X11 pixels, not a catalog fixture.
use fr_core::limits::ProtocolLimits;
use fr_native::{NativeError, displays::X11Inventory};
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
struct Screen {
    child: Child,
    name: String,
    display: *mut c_void,
    root: c_ulong,
}
impl Screen {
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
    fn atom(&self, name: &str) -> c_ulong {
        let n = CString::new(name).unwrap();
        unsafe { XInternAtom(self.display, n.as_ptr(), 0) }
    }
    fn monitor(&self, name: &str, x: i32, y: i32, width: i32, height: i32) {
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
    fn remove(&self, name: &str) {
        unsafe {
            XRRDeleteMonitor(self.display, self.root, self.atom(name));
            XSync(self.display, 0);
        }
    }
    fn paint(&self, x: i32, width: u32, color: c_ulong) {
        unsafe {
            let gc = XCreateGC(self.display, self.root, 0, core::ptr::null_mut());
            assert!(!gc.is_null());
            XSetForeground(self.display, gc, color);
            XFillRectangle(self.display, self.root, gc, x, 0, width, 480);
            XFreeGC(self.display, gc);
            XSync(self.display, 0);
        }
    }
    fn inventory(&self) -> X11Inventory {
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
#[test]
fn actual_randr_catalog_and_unchanged_geometry_are_stable() {
    let screen = Screen::start();
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    assert_eq!(catalog.displays().len(), 1);
    let d = catalog.displays()[0];
    assert_eq!((d.x, d.y, d.pixel_width, d.pixel_height), (0, 0, 640, 480));
    for _ in 0..10 {
        assert_eq!(inventory.catalog().unwrap(), catalog);
    }
    assert_eq!(format!("{catalog:?}"), "DisplayCatalog { count: 1, .. }");
}
#[test]
fn selected_monitor_copies_only_its_pixels_not_the_surrounding_root() {
    let screen = Screen::start();
    screen.monitor("fr-left", 0, 0, 320, 480);
    screen.monitor("fr-right", 320, 0, 320, 480);
    screen.paint(0, 320, 0x00ff_0000);
    screen.paint(320, 320, 0x0000_00ff);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let selected = catalog.displays().iter().find(|d| d.x == 320).unwrap();
    let mut capture = inventory.select(selected.handle).unwrap();
    let frame = capture.snapshot().unwrap();
    assert_eq!((frame.width(), frame.height()), (320, 480));
    assert!(
        frame
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [255, 0, 0, 255])
    );
    screen.paint(0, 320, 0x0000_ff00);
    assert_eq!(capture.snapshot().unwrap().pixels(), frame.pixels());
}
#[test]
fn changed_monitor_is_terminal_even_after_bounds_are_restored() {
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let selected = catalog
        .displays()
        .iter()
        .find(|d| d.pixel_width == 320)
        .unwrap();
    let mut capture = inventory.select(selected.handle).unwrap();
    capture.snapshot().unwrap();
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 320, 0, 320, 480);
    assert_eq!(capture.snapshot().err(), Some(NativeError::GeometryChanged));
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(capture.snapshot().err(), Some(NativeError::Closed));
}
#[test]
fn remove_readd_with_identical_identity_and_size_does_not_revive_inventory() {
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let original = inventory.catalog().unwrap();
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    // Final metadata is identical. The queued native resource notification must
    // still fence the old lifetime instead of using equality as proof of identity.
    let fresh = screen.inventory().catalog().unwrap();
    assert_eq!(fresh, original);
    assert_eq!(
        inventory.catalog().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(inventory.catalog().err(), Some(NativeError::Closed));
}
#[test]
fn out_of_scope_handles_and_network_or_malformed_display_selectors_refuse() {
    for name in [
        "",
        ":",
        ":..0",
        ":0.",
        ":0.1.2",
        "127.0.0.1:0",
        "unix:0",
        ":0\0",
    ] {
        assert!(X11Inventory::open(name, ProtocolLimits::ABSOLUTE).is_err());
    }
    let screen = Screen::start();
    assert_eq!(
        screen.inventory().select(u128::MAX).err().unwrap(),
        NativeError::InvalidConfiguration
    );
}
#[test]
fn inventory_capacity_is_enforced_instead_of_truncating_displays() {
    let screen = Screen::start();
    for i in 0..9 {
        screen.monitor(&format!("fr-{i}"), 0, 0, 160, 120);
    }
    assert_eq!(
        X11Inventory::open(&screen.name, ProtocolLimits::ABSOLUTE).err(),
        Some(NativeError::InvalidConfiguration)
    );
}

#[test]
fn topology_changed_after_encode_submission_cannot_release_the_old_picture() {
    use fr_core::ids::CodecConfigurationGeneration;
    use fr_media::{
        access_unit::FrameId,
        worker::{Backend, Configuration},
    };
    use fr_native::{
        EncodeBackend, HevcEncoder,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let display = catalog
        .displays()
        .iter()
        .find(|d| d.pixel_width == 320)
        .unwrap();
    let surface = inventory.select(display.handle).unwrap();
    let config = Configuration {
        width: 320,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let encoder = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(config.fps),
        config.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::selected(surface, encoder);
    assert_eq!(
        capture.capture(FrameId::FIRST, 100, true, false).unwrap(),
        CaptureOutput::Submitted
    );
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(
        capture.poll_output().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(capture.poll_output().err(), Some(NativeError::Closed));
    assert_eq!(
        capture
            .capture(FrameId::from_raw(2), 200, false, true)
            .err(),
        Some(NativeError::Closed)
    );
}

#[path = "display_inventory/worker.rs"]
mod selected_worker;

#[test]
fn full_screen_capture_also_fences_topology_changes_after_encode_submission() {
    use fr_core::ids::CodecConfigurationGeneration;
    use fr_media::{
        access_unit::FrameId,
        worker::{Backend, Configuration},
    };
    use fr_native::{
        EncodeBackend, HevcEncoder,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let surface =
        fr_native::X11Surface::capture(Some(&screen.name), ProtocolLimits::ABSOLUTE).unwrap();
    let config = Configuration {
        width: 640,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let encoder = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(config.fps),
        config.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::new(surface, encoder);
    assert_eq!(
        capture.capture(FrameId::FIRST, 100, true, false).unwrap(),
        CaptureOutput::Submitted
    );
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(
        capture.poll_output().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(capture.poll_output().err(), Some(NativeError::Closed));
    assert_eq!(
        capture
            .capture(FrameId::from_raw(2), 200, false, true)
            .err(),
        Some(NativeError::Closed)
    );
}
