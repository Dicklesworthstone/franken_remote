//! Actual XCB owner/Xlib peer, original Viewer over localhost TLS/UDP. Native
//! mapping, pixel submission and cancellation are not tailnet/approval/scanout
//! evidence. No input grant or media completion is fabricated in these tests.
#![cfg(all(target_os = "linux", feature = "linux-viewer-window"))]
#![forbid(unsafe_code)]
use asupersync::{runtime::Runtime, types::Budget};
use fr_core::limits::ProtocolLimits;
use fr_native::viewer_window::{Error, Status, StopReason, ViewerWindow, WindowControl};
use fr_transport::quic::{ALPN, Policy};
use fr_wire::negotiation::{Offer, Role};
use frd::session_startup::Viewer;
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod support;
struct Desktop {
    process: Child,
    display: String,
}
impl Desktop {
    fn start() -> Self {
        let mut process = Command::new("Xvfb")
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
        BufReader::new(process.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            process,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
    fn peer(&self, id: u32, op: &str) -> String {
        let result = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/viewer_window/peer.py"))
            .args([&self.display, &id.to_string(), op])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "peer {op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
fn viewer(runtime: &Runtime) -> Viewer {
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (client, server) = runtime.block_on(support::native_pair(&cx, "localhost", ALPN));
    let viewer = Viewer::new(
        cx,
        client.unwrap(),
        Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![],
        },
        Policy::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    // The control handle must work even before application admission finishes.
    // Closing this idle peer does not pump/rewrite the local Viewer lifetime.
    drop(server.unwrap());
    viewer
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < until, "native deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
fn mapped(window: &ViewerWindow) -> (WindowControl, u32) {
    let c = window.control();
    wait(|| c.status() != Status::Opening);
    assert_eq!(c.status(), Status::Mapped);
    let id = c.target().unwrap().window();
    assert_ne!(id, 0);
    (c, id)
}
fn finish(window: &mut ViewerWindow, expected: StopReason) {
    wait(|| window.finish().is_some());
    assert_eq!(window.finish(), Some(expected));
    assert_eq!(window.finish(), Some(expected));
}
#[test]
fn original_window_is_shared_by_decoder_and_input_without_a_new_session() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    let input = control.input_window().unwrap();
    assert_eq!((input.id, input.width, input.height), (id, 320, 240));
    assert_eq!(desktop.peer(id, "exists"), "1");
    assert!(
        window
            .decoder_launch(Path::new("/qualified/package/fr-media-worker"), None, 17)
            .is_ok()
    );
    assert!(window.finish().is_none());
    desktop.peer(id, "move");
    thread::sleep(Duration::from_millis(25));
    assert_eq!(control.status(), Status::Mapped);
    assert!(!original.is_stopped());
    control.stop();
    assert!(
        original.is_stopped(),
        "fence must not wait for the native thread"
    );
    assert_eq!(control.target(), Err(Error::NotReady));
    finish(&mut window, StopReason::User);
    assert_eq!(desktop.peer(id, "exists"), "0");
}
#[test]
fn resize_hide_destroy_and_window_manager_close_fence_the_original_viewer() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    for (op, reason) in [
        ("resize", StopReason::GeometryChanged),
        ("unmap", StopReason::Hidden),
        // X11 destroys a mapped window by unmapping it first. Preserve the
        // FIRST terminal reason rather than rewriting it after native cleanup.
        ("destroy", StopReason::Hidden),
        ("close", StopReason::User),
    ] {
        let session = viewer(&runtime);
        let original = session.control();
        let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
        let (control, id) = mapped(&window);
        desktop.peer(id, op);
        wait(|| matches!(control.status(), Status::Stopped(_)));
        assert!(original.is_stopped());
        assert_eq!(control.status(), Status::Stopped(reason));
        finish(&mut window, reason);
        assert_eq!(control.target(), Err(Error::NotReady));
    }
}
#[test]
fn stop_and_drop_do_not_cancel_a_foreign_viewer() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let foreign = viewer(&runtime);
    let original = session.control();
    let window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    drop(window);
    assert!(original.is_stopped());
    assert!(!foreign.control().is_stopped());
    assert_eq!(control.status(), Status::Stopped(StopReason::OwnerDropped));
    wait(|| desktop.peer(id, "exists") == "0");
}
#[test]
fn cancellation_from_before_native_startup_is_terminal() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    original.stop();
    assert!(matches!(
        ViewerWindow::start(&desktop.display, 320, 240, original),
        Err(Error::SessionEnded)
    ));
}
#[test]
fn session_close_and_failed_native_open_are_collected_without_replacement() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, _) = mapped(&window);
    drop(session);
    assert!(original.is_stopped());
    assert_eq!(control.status(), Status::Stopped(StopReason::SessionEnded));
    finish(&mut window, StopReason::SessionEnded);
    let session = viewer(&runtime);
    let original = session.control();
    let mut absent = ViewerWindow::start(":65534", 320, 240, original.clone()).unwrap();
    finish(&mut absent, StopReason::NativeFailure);
    assert!(original.is_stopped());
}
#[test]
fn malformed_local_choices_refuse_before_native_work_and_fence_original() {
    let runtime = support::runtime();
    for (display, width, height, expected) in [
        ("localhost:0", 320, 240, Error::InvalidDisplay),
        (":1..2", 320, 240, Error::InvalidDisplay),
        (":0", 319, 240, Error::InvalidSize),
        (":0", 0, 240, Error::InvalidSize),
        (":0", 320, u32::MAX, Error::InvalidSize),
    ] {
        let session = viewer(&runtime);
        let original = session.control();
        assert!(
            matches!(ViewerWindow::start(display, width, height, original.clone()), Err(e) if e == expected)
        );
        assert!(original.is_stopped());
    }
}
#[cfg(feature = "linux-media")]
#[test]
fn actual_selected_drawable_receives_pixels_and_outlives_its_borrowed_presenter() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, session.control()).unwrap();
    let (control, id) = mapped(&window);
    let mut presenter = fr_native::X11Surface::present_in(
        Some(&desktop.display),
        control.target().unwrap(),
        ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    let frame = fr_native::BgraFrame::new(
        320,
        240,
        [0x31, 0x72, 0xb4, 0xff].repeat(320 * 240),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    presenter.present(&frame).unwrap();
    assert_eq!(desktop.peer(id, "pixel"), 0xb4_72_31u32.to_string());
    drop(presenter);
    assert_eq!(desktop.peer(id, "exists"), "1");
    assert_eq!(desktop.peer(id, "pixel"), 0xb4_72_31u32.to_string());
    assert!(!session.control().is_stopped());
    control.stop();
    finish(&mut window, StopReason::User);
}

#[test]
fn native_map_and_stop_wake_the_async_owner_without_polling_a_timer() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, session.control()).unwrap();
    let target = runtime.block_on(window.ready()).unwrap();
    assert_eq!(window.control().target().unwrap(), target);
    assert!(!session.control().is_stopped());
    window.control().stop();
    assert_eq!(
        runtime.block_on(window.ready()),
        Err(Error::NativeStopped(StopReason::User))
    );
    finish(&mut window, StopReason::User);
}

#[test]
fn abandoned_map_wait_unregisters_without_closing_the_retained_window() {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Wake, Waker};
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, session.control()).unwrap();
    let count = Arc::new(Counter(AtomicUsize::new(0)));
    let wake = Waker::from(count.clone());
    {
        let mut ready = pin!(window.ready());
        let _ = ready.as_mut().poll(&mut Context::from_waker(&wake));
    }
    // A different executor can await the same retained owner after cancellation.
    let target = runtime.block_on(window.ready()).unwrap();
    assert_eq!(window.control().target().unwrap(), target);
    assert!(!session.control().is_stopped());
    window.control().stop();
    finish(&mut window, StopReason::User);
    // Native completion and terminal stop did not retain the discarded executor.
    assert_eq!(Arc::strong_count(&count), 2);
}

#[cfg(feature = "linux-desktop")]
#[path = "viewer_window/desktop.rs"]
mod desktop;

#[path = "viewer_window/picker.rs"]
mod picker;
