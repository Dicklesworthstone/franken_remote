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
#[allow(dead_code, clippy::duplicate_mod)]
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

#[test]
fn close_intent_survives_session_cancellation_before_cleanup_sampling() {
    let display = Display::new();
    for requested in [false, true] {
        let (_rt, viewer, mut window) = window(&display.name);
        let control = window.control();
        let id = control.target().unwrap().window();
        control.enable_orderly_close().unwrap();
        assert!(!control.close_requested());
        if requested {
            display.event(id, "close");
            wait(|| control.close_requested());
            assert_eq!(control.status(), Status::CloseRequested);
        }

        // The reconnect supervisor cancels the original viewer context before
        // its Cleaning callback. Reproduce that real cancellation here, before
        // the CLI can sample the original window's retained local intent.
        viewer.control().stop();
        assert_eq!(control.status(), Status::Stopped(StopReason::SessionEnded));
        assert_eq!(control.close_requested(), requested);
        wait(|| window.finish().is_some());
        assert_eq!(window.finish(), Some(StopReason::SessionEnded));
        // Reaping never turns cancellation into a request or erases a real one.
        assert_eq!(control.close_requested(), requested);
    }
}

#[test]
fn unarmed_wm_close_retains_intent_but_programmatic_stop_does_not() {
    let display = Display::new();
    for requested in [false, true] {
        let (_rt, viewer, mut window) = window(&display.name);
        let control = window.control();
        let id = control.target().unwrap().window();
        assert!(!control.close_requested());
        // Do not arm orderly close. The native WM branch must still publish
        // its own intent before immediate cancellation and thread retirement.
        if requested {
            display.event(id, "close");
            wait(|| control.close_requested());
        } else {
            control.stop();
        }
        wait(|| viewer.control().is_stopped());
        wait(|| window.finish().is_some());
        if requested {
            // User publication and another thread observing cancellation can
            // race. Neither legitimate terminal order starts a close grace.
            assert!(matches!(
                window.finish(),
                Some(StopReason::User | StopReason::SessionEnded)
            ));
        }
        // A programmatic cleanup stop cannot impersonate a WM request.
        assert_eq!(control.close_requested(), requested);
        viewer.control().stop();
        let _ = control.status();
        assert_eq!(control.close_requested(), requested);
    }
}

#[test]
fn real_resize_fences_old_target_then_coalesces_one_checked_native_size() {
    let display = Display::new();
    for (event, expected) in [
        ("resize", (322, 240)),
        ("resize_burst", (400, 300)),
        ("resize_return", (320, 240)),
    ] {
        let (_rt, viewer, mut window) = window(&display.name);
        let control = window.control();
        let id = control.target().unwrap().window();
        control.enable_resize(640, 480).unwrap();
        display.event(id, event);
        wait(|| matches!(control.status(), Status::Stopped(_)));
        assert!(
            viewer.control().is_stopped(),
            "terminal status preceded input fence"
        );
        assert_eq!(control.target(), Err(Error::NotReady));
        wait(|| window.finish().is_some());
        assert_eq!(window.finish(), Some(StopReason::GeometryChanged));
        assert_eq!(control.resize_requested(), Some(expected));
        // A retained receipt is not permission to override a later local stop.
        control.stop();
        assert_eq!(control.resize_requested(), None);
    }
}

#[test]
fn synthetic_invalid_and_negative_lifecycle_events_never_supply_replacement_intent() {
    let display = Display::new();
    for event in [
        "synthetic_resize",
        "resize_synthetic",
        "resize_odd",
        "resize_unmap",
        "resize_destroy",
        "resize_close",
    ] {
        let (_rt, viewer, mut window) = window(&display.name);
        let control = window.control();
        let id = control.target().unwrap().window();
        control.enable_resize(640, 480).unwrap();
        display.event(id, event);
        wait(|| window.finish().is_some());
        assert!(viewer.control().is_stopped());
        assert_eq!(control.resize_requested(), None, "{event}");
    }
}

#[test]
fn input_side_fence_without_window_confirmation_cannot_reopen() {
    let display = Display::new();
    let (_rt, viewer, mut window) = window(&display.name);
    let control = window.control();
    control.enable_resize(640, 480).unwrap();
    // This is only a tentative receipt from the separate input connection.
    // No ConfigureNotify was delivered to the original window owner.
    assert!(control.record_resize(322, 240));
    assert!(viewer.control().is_stopped());
    assert_eq!(control.resize_requested(), None);
    wait(|| window.finish().is_some());
    assert_eq!(control.resize_requested(), None);
    assert_eq!(window.finish(), Some(StopReason::GeometryChanged));
}

#[test]
fn prior_stop_and_disabled_resize_cannot_acquire_a_replacement_request() {
    let display = Display::new();
    for prior in [None, Some(StopReason::Hidden), Some(StopReason::FocusLost)] {
        let (_rt, viewer, mut window) = window(&display.name);
        let control = window.control();
        if let Some(reason) = prior {
            control.enable_resize(640, 480).unwrap();
            control.0.stop(reason);
        }
        assert!(!control.record_resize(322, 240));
        control.stop();
        wait(|| window.finish().is_some());
        assert!(viewer.control().is_stopped());
        assert_eq!(control.resize_requested(), None);
    }
}

#[test]
fn published_display_choice_cancels_resize_without_confusing_offered_capability() {
    let display = Display::new();
    let (_rt, viewer, mut window) = window(&display.name);
    let control = window.control();
    control.enable_resize(640, 480).unwrap();
    control
        .set_display_switch_mode(DisplaySwitchMode::Controlled)
        .unwrap();
    assert!(
        control.resize_enabled(),
        "offered capability is not a choice"
    );
    let scope = control.display_switch_scope().unwrap();
    assert!(control.request_display_switch(scope, Instant::now()));
    assert!(
        !control.resize_enabled(),
        "published choice did not cancel resize"
    );
    assert!(!control.record_resize(322, 240));
    control.stop();
    wait(|| window.finish().is_some());
    assert!(viewer.control().is_stopped());
    assert_eq!(control.resize_requested(), None);
}
