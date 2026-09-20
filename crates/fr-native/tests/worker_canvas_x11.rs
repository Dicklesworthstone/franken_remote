#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual X11 exposure of the production presenter. No callback manufactures a
//! presentation witness, source timestamp, decoded receipt, or input authority.
use fr_core::limits::ProtocolLimits;
use fr_native::{BgraFrame, NativeError, X11Surface};
use std::{
    ffi::{CString, c_char, c_int, c_uint, c_ulong, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    ptr::NonNull,
};

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XClearArea(
        display: *mut c_void,
        window: c_ulong,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        exposures: c_int,
    ) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XCreateSimpleWindow(
        display: *mut c_void,
        parent: c_ulong,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        border_width: c_uint,
        border: c_ulong,
        background: c_ulong,
    ) -> c_ulong;
    fn XMapRaised(display: *mut c_void, window: c_ulong) -> c_int;
    fn XUnmapWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XDestroyWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XResizeWindow(display: *mut c_void, window: c_ulong, width: c_uint, height: c_uint)
    -> c_int;
    fn XGetImage(
        display: *mut c_void,
        drawable: c_ulong,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        plane_mask: c_ulong,
        format: c_int,
    ) -> *mut c_void;
    fn XGetPixel(image: *mut c_void, x: c_int, y: c_int) -> c_ulong;
    fn XDestroyImage(image: *mut c_void) -> c_int;
    fn XMoveWindow(d: *mut c_void, w: c_ulong, x: c_int, y: c_int) -> c_int;
    fn XReparentWindow(d: *mut c_void, w: c_ulong, parent: c_ulong, x: c_int, y: c_int) -> c_int;
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
                "128x128x24",
                "-nolisten",
                "tcp",
                "-noreset",
                "-bs",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u32>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Observer(NonNull<c_void>);
impl Observer {
    fn open(server: &Server) -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let name = CString::new(server.display.as_str()).unwrap();
        // SAFETY: the local test owns this distinct connection until Drop.
        Self(NonNull::new(unsafe { XOpenDisplay(name.as_ptr()) }).unwrap())
    }
    fn expose(&mut self, window: u32) {
        // SAFETY: live test-owned connection; this request destroys existing
        // window contents using the server's background, then emits Expose.
        // No second Present/capture request is issued by the test.
        unsafe {
            XClearArea(self.0.as_ptr(), window.into(), 0, 0, 0, 0, 1);
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn cover_then_uncover(&mut self) {
        // SAFETY: all requests use our live connection and a distinct local
        // overlay that this helper alone creates and destroys. Backing store is
        // disabled in the server so the exposed presenter must restore itself.
        unsafe {
            let root = XDefaultRootWindow(self.0.as_ptr());
            let cover =
                XCreateSimpleWindow(self.0.as_ptr(), root, 0, 0, 128, 128, 0, 0, 0x00ff_ffff);
            assert_ne!(cover, 0);
            XMapRaised(self.0.as_ptr(), cover);
            XSync(self.0.as_ptr(), 0);
            XUnmapWindow(self.0.as_ptr(), cover);
            XSync(self.0.as_ptr(), 0);
            XDestroyWindow(self.0.as_ptr(), cover);
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn unmap_then_remap(&mut self, window: u32) {
        // SAFETY: local test controls the exported target; both requests and
        // barrier complete before its original renderer is next serviced.
        unsafe {
            XUnmapWindow(self.0.as_ptr(), window.into());
            XMapRaised(self.0.as_ptr(), window.into());
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn resize_away_and_back(&mut self, window: u32) {
        // SAFETY: only this test's selected 32x32 window is reconfigured.
        unsafe {
            XResizeWindow(self.0.as_ptr(), window.into(), 48, 48);
            XResizeWindow(self.0.as_ptr(), window.into(), 32, 32);
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn destroy_target(&mut self, window: u32) {
        // SAFETY: the test controls this local target; destruction is complete
        // before the owning/attached presentation connections are dropped.
        unsafe {
            XDestroyWindow(self.0.as_ptr(), window.into());
            XSync(self.0.as_ptr(), 0);
        }
    }
    fn children(&mut self, window: u32) -> Vec<u32> {
        let (mut root, mut parent, mut children, mut count) = (0, 0, std::ptr::null_mut(), 0);
        // SAFETY: writable scalar outputs and live local test window. The
        // returned child array is copied with its exact Xlib length and freed.
        let status = unsafe {
            XQueryTree(
                self.0.as_ptr(),
                window.into(),
                &raw mut root,
                &raw mut parent,
                &raw mut children,
                &raw mut count,
            )
        };
        assert_ne!(status, 0);
        let result = if count == 0 {
            Vec::new()
        } else {
            assert!(count <= 8, "renderer created unbounded child windows");
            assert!(!children.is_null());
            // SAFETY: XQueryTree returned exactly count initialized Window IDs.
            unsafe { std::slice::from_raw_parts(children, count as usize) }
                .iter()
                .map(|id| u32::try_from(*id).unwrap())
                .collect()
        };
        if !children.is_null() {
            // SAFETY: returned array belongs to Xlib and is released only once.
            unsafe {
                XFree(children.cast());
            }
        }
        result
    }
    fn wait_for_canvas_removal(&mut self, window: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            if self.children(window).is_empty() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker connection retained its canvas after exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    fn assert_black(&mut self, window: u32) {
        struct Image(NonNull<c_void>);
        impl Drop for Image {
            fn drop(&mut self) {
                // SAFETY: unique image from XGetImage, no retained pixel borrows.
                unsafe {
                    XDestroyImage(self.0.as_ptr());
                }
            }
        }
        // SAFETY: independent readback of the still-live test window. All
        // queried coordinates are inside this 32x32 allocation (ZPixmap=2).
        let image = Image(
            NonNull::new(unsafe {
                XGetImage(
                    self.0.as_ptr(),
                    window.into(),
                    0,
                    0,
                    32,
                    32,
                    c_ulong::MAX,
                    2,
                )
            })
            .unwrap(),
        );
        for y in 0..32 {
            for x in 0..32 {
                // SAFETY: exact in-bounds coordinates in the owned XImage.
                assert_eq!(
                    unsafe { XGetPixel(image.0.as_ptr(), x, y) },
                    0,
                    "retired remote pixels remain visible or retained for exposure"
                );
            }
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: unique test connection; no native owners borrow it.
        unsafe {
            XCloseDisplay(self.0.as_ptr());
        }
    }
}
fn pattern(width: u32, height: u32, frame: u8) -> BgraFrame {
    let mut bytes = Vec::new();
    for y in 0..height {
        for x in 0..width {
            bytes.extend_from_slice(&[
                u8::try_from(x).unwrap(),
                u8::try_from(y).unwrap(),
                frame,
                255,
            ]);
        }
    }
    BgraFrame::new(width, height, bytes, &ProtocolLimits::ABSOLUTE).unwrap()
}
#[test]
fn canvas_repairs_exposure_using_original_single_image_accounting() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let children = observer.children(target.window());
    assert_eq!(children.len(), 1);
    let canvas = children[0];
    for value in [17, 93, 241] {
        let pixels = pattern(32, 32, value);
        renderer.present(&pixels).unwrap();
        observer.expose(canvas);
        observer.cover_then_uncover();
        let repair = renderer.maintain_presentation().unwrap();
        assert!(repair.repainted);
        assert_eq!(repair.retained_bytes, 32 * 32 * 4);
        assert!(owner.snapshot().unwrap().pixels() == pixels.pixels());
        let quiet = renderer.maintain_presentation().unwrap();
        assert!(!quiet.repainted);
        assert_eq!(quiet.retained_bytes, repair.retained_bytes);
        assert_eq!(observer.children(target.window()), children);
    }
}
#[test]
fn unpresented_canvas_has_no_retained_pixel_allocation() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let children = observer.children(target.window());
    assert_eq!(children.len(), 1);
    observer.expose(children[0]);
    let repair = renderer.maintain_presentation().unwrap();
    assert!(!repair.repainted);
    assert_eq!(repair.retained_bytes, 0);
    observer.assert_black(target.window());
    drop(renderer);
    observer.wait_for_canvas_removal(target.window());
    assert_eq!(owner.presentation_target().unwrap(), target);
}
#[test]
fn canvas_resize_retires_only_renderer_and_cannot_resurrect_it() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let pixels = pattern(32, 32, 187);
    renderer.present(&pixels).unwrap();
    let canvas = observer.children(target.window())[0];
    observer.resize_away_and_back(canvas);
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    observer.wait_for_canvas_removal(target.window());
    observer.assert_black(target.window());
    assert_eq!(renderer.present(&pixels), Err(NativeError::GeometryChanged));
    assert_eq!(owner.presentation_target().unwrap(), target);
    // A fresh explicit native attachment is possible, but cannot revive the old
    // instance. This factory creates no input grant or remote observation scope.
    let mut replacement =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    replacement.present(&pixels).unwrap();
    assert!(owner.snapshot().unwrap().pixels() == pixels.pixels());
    assert_eq!(renderer.present(&pixels), Err(NativeError::GeometryChanged));
}
#[test]
fn canvas_unmap_remap_closes_attachment_and_keeps_original_window() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    renderer.present(&pattern(32, 32, 211)).unwrap();
    let canvas = observer.children(target.window())[0];
    observer.unmap_then_remap(canvas);
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    observer.wait_for_canvas_removal(target.window());
    observer.assert_black(target.window());
    assert_eq!(owner.presentation_target().unwrap(), target);
}
#[test]
fn destroyed_parent_does_not_trigger_duplicate_window_requests() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    renderer.present(&pattern(32, 32, 23)).unwrap();
    observer.destroy_target(target.window());
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        owner.presentation_target(),
        Err(NativeError::GeometryChanged)
    );
    drop(renderer);
    drop(owner);
}
#[test]
fn dropping_an_old_attachment_does_not_erase_its_replacement() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut old =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    old.present(&pattern(32, 32, 91)).unwrap();
    let mut current =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let pixels = pattern(32, 32, 203);
    current.present(&pixels).unwrap();
    assert_eq!(observer.children(target.window()).len(), 2);
    drop(old);
    let children = observer.children(target.window());
    assert_eq!(children.len(), 1);
    observer.expose(children[0]);
    current.maintain_presentation().unwrap();
    assert!(owner.snapshot().unwrap().pixels() == pixels.pixels());
    drop(current);
    observer.wait_for_canvas_removal(target.window());
    observer.assert_black(target.window());
}
#[test]
fn parent_resize_away_and_back_remains_terminal() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut owner =
        X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
    let target = owner.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let pixels = pattern(32, 32, 253);
    renderer.present(&pixels).unwrap();
    observer.resize_away_and_back(target.window());
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    observer.wait_for_canvas_removal(target.window());
    observer.assert_black(target.window());
    assert_eq!(renderer.present(&pixels), Err(NativeError::GeometryChanged));
    assert_eq!(
        owner.presentation_target(),
        Err(NativeError::GeometryChanged)
    );
}
#[path = "worker_canvas_x11/worker.rs"]
mod worker;

#[path = "worker_canvas_x11/input.rs"]
mod input;

#[test]
fn canvas_move_or_reparent_away_and_back_cannot_restore_the_mapping() {
    for reparent in [false, true] {
        let server = Server::start();
        let mut observer = Observer::open(&server);
        let mut owner =
            X11Surface::presenter(Some(&server.display), 32, 32, ProtocolLimits::ABSOLUTE).unwrap();
        let target = owner.presentation_target().unwrap();
        let mut renderer =
            X11Surface::present_in(Some(&server.display), target, ProtocolLimits::ABSOLUTE)
                .unwrap();
        renderer.present(&pattern(32, 32, 89)).unwrap();
        let children = observer.children(target.window());
        assert_eq!(children.len(), 1);
        // SAFETY: the independent fixture changes only this renderer's child.
        // Both transitions finish before validation; final dimensions and
        // coordinates are identical, but its original mapping is no longer valid.
        unsafe {
            let d = observer.0.as_ptr();
            let child = c_ulong::from(children[0]);
            if reparent {
                XReparentWindow(d, child, XDefaultRootWindow(d), 0, 0);
                XReparentWindow(d, child, target.window().into(), 0, 0);
            } else {
                XMoveWindow(d, child, 1, 1);
                XMoveWindow(d, child, 0, 0);
            }
            XSync(d, 0);
        }
        assert_eq!(
            renderer.maintain_presentation(),
            Err(NativeError::GeometryChanged)
        );
        assert_eq!(
            renderer.present(&pattern(32, 32, 90)),
            Err(NativeError::GeometryChanged)
        );
        observer.wait_for_canvas_removal(target.window());
        observer.assert_black(target.window());
        assert_eq!(owner.presentation_target().unwrap(), target);
    }
}
