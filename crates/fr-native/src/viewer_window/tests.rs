//! Real XCB window and TLS/UDP control handle; private arming is a policy fixture.
use super::*;
use asupersync::types::Budget;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{ALPN, Policy};
use fr_wire::negotiation::{Offer, Role};
use frd::session_startup::Viewer;
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};
#[allow(dead_code)]
#[path = "../../../fr-transport/tests/support/mod.rs"]
mod support;
struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn new() -> Self {
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
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
    fn event(&self, id: u32, event: &str) {
        let status = Command::new("/usr/bin/python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/viewer_window/peer.py"))
            .args([&self.name, &id.to_string(), event])
            .status()
            .unwrap();
        assert!(status.success());
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn window(display: &str) -> (asupersync::runtime::Runtime, Viewer, ViewerWindow) {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (client, server) = rt.block_on(support::native_pair(&cx, "localhost", ALPN));
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
    drop(server.unwrap());
    let mut window = ViewerWindow::start(display, 320, 240, viewer.control()).unwrap();
    rt.block_on(window.ready()).unwrap();
    (rt, viewer, window)
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn orderly_request_retires_target_but_not_socket_and_direct_stop_still_wins() {
    let display = Display::new();
    let (_rt, viewer, mut window) = window(&display.name);
    let c = window.control();
    let id = c.target().unwrap().window();
    c.enable_orderly_close().unwrap();
    display.event(id, "close");
    wait(|| c.status() == Status::CloseRequested);
    assert!(!viewer.control().is_stopped());
    assert_eq!(c.target(), Err(Error::NotReady));
    assert!(window.finish().is_none());
    c.stop();
    assert!(viewer.control().is_stopped());
    wait(|| window.finish().is_some());
    assert_eq!(window.finish(), Some(StopReason::User));
}
#[test]
fn repeated_requests_cannot_renew_the_original_native_fallback() {
    let display = Display::new();
    let (_rt, viewer, mut window) = window(&display.name);
    let c = window.control();
    let id = c.target().unwrap().window();
    c.enable_orderly_close().unwrap();
    display.event(id, "close");
    wait(|| c.status() == Status::CloseRequested);
    let started = Instant::now();
    for _ in 0..3 {
        thread::sleep(Duration::from_millis(90));
        display.event(id, "close");
        assert_eq!(c.status(), Status::CloseRequested);
    }
    wait(|| window.finish().is_some());
    assert_eq!(window.finish(), Some(StopReason::CloseExpired));
    assert!(viewer.control().is_stopped());
    assert!(started.elapsed() < Duration::from_millis(750));
}
#[test]
fn negative_lifecycle_and_owner_drop_never_wait_for_orderly_closure() {
    let display = Display::new();
    for op in ["resize", "unmap"] {
        let (_rt, viewer, mut window) = window(&display.name);
        let c = window.control();
        let id = c.target().unwrap().window();
        c.enable_orderly_close().unwrap();
        display.event(id, "close");
        wait(|| c.status() == Status::CloseRequested);
        display.event(id, op);
        wait(|| window.finish().is_some());
        assert_eq!(
            window.finish(),
            Some(if op == "resize" {
                StopReason::GeometryChanged
            } else {
                StopReason::Hidden
            })
        );
        assert!(viewer.control().is_stopped());
    }
    let (_rt, viewer, window) = window(&display.name);
    let c = window.control();
    c.enable_orderly_close().unwrap();
    drop(window);
    assert!(viewer.control().is_stopped());
    assert_eq!(c.status(), Status::Stopped(StopReason::OwnerDropped));
}
