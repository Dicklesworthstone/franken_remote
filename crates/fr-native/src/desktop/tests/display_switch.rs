//! Actual window/intent state with real XCB and authenticated TLS/UDP owners.
//! Publishing local intent here exercises arbitration, not input authorization.
use super::super::refresh_display_switch;
use crate::viewer_window::{DisplaySwitchMode, Error, Status, ViewerWindow, WindowControl};
use asupersync::{runtime::Runtime, types::Budget};
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{ALPN, Policy};
use fr_wire::negotiation::{Offer, Role};
use frd::session_startup::Viewer;
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[allow(dead_code)]
#[path = "../../../../fr-transport/tests/support/mod.rs"]
mod support;

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start() -> Self {
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
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Fixture {
    _runtime: Runtime,
    viewer: Viewer,
    window: ViewerWindow,
    display: Display,
}
impl Fixture {
    fn start() -> Self {
        let display = Display::start();
        let runtime = support::runtime();
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
        drop(server.unwrap());
        let mut window = ViewerWindow::start(&display.name, 320, 240, viewer.control()).unwrap();
        runtime.block_on(window.ready()).unwrap();
        window.control().enable_orderly_close().unwrap();
        window
            .control()
            .set_display_switch_mode(DisplaySwitchMode::Controlled)
            .unwrap();
        Self {
            _runtime: runtime,
            viewer,
            window,
            display,
        }
    }
    fn control(&self) -> WindowControl {
        self.window.control()
    }
    fn choose(&self) {
        let control = self.control();
        let scope = control.display_switch_scope().unwrap();
        assert!(control.request_display_switch(scope, Instant::now()));
        assert_eq!(control.status(), Status::DisplaySwitchRequested);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.window.control().stop();
        let until = Instant::now() + Duration::from_secs(2);
        while self.window.finish().is_none() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn native_choice_between_status_and_mode_refresh_keeps_orderly_close() {
    let fixture = Fixture::start();
    let control = fixture.control();
    // Deterministically publish on the actual Intent after the UI's status
    // read, before its mode refresh. No timing-dependent thread race is needed.
    assert_eq!(control.status(), Status::Mapped);
    fixture.choose();
    assert_eq!(
        control.set_display_switch_mode(DisplaySwitchMode::Controlled),
        Err(Error::NotReady),
    );
    assert_eq!(
        refresh_display_switch(
            &control,
            DisplaySwitchMode::Controlled,
            DisplaySwitchMode::Controlled,
        ),
        Ok(true),
    );
    assert_eq!(control.status(), Status::DisplaySwitchRequested);
    assert!(!fixture.viewer.control().is_stopped());
}

#[test]
fn another_interactive_mode_cannot_consume_the_previous_modes_choice() {
    for next in [DisplaySwitchMode::Viewing, DisplaySwitchMode::Disabled] {
        let fixture = Fixture::start();
        fixture.choose();
        assert_eq!(
            refresh_display_switch(&fixture.control(), DisplaySwitchMode::Controlled, next),
            Err(Error::NotReady),
        );
    }
}

#[test]
fn obsolete_scope_and_stopped_session_cannot_become_local_close() {
    let fixture = Fixture::start();
    let control = fixture.control();
    let original = control.display_switch_scope().unwrap();
    for (previous, next) in [
        (DisplaySwitchMode::Controlled, DisplaySwitchMode::Viewing),
        (DisplaySwitchMode::Viewing, DisplaySwitchMode::Controlled),
    ] {
        assert_eq!(refresh_display_switch(&control, previous, next), Ok(false));
    }
    assert!(!control.request_display_switch(original, Instant::now()));
    assert_eq!(control.status(), Status::Mapped);
    fixture.viewer.control().stop();
    assert_eq!(
        refresh_display_switch(
            &control,
            DisplaySwitchMode::Controlled,
            DisplaySwitchMode::Controlled,
        ),
        Err(Error::NotReady),
    );
    assert!(matches!(control.status(), Status::Stopped(_)));
}

#[test]
fn a_mapped_but_cancelled_intent_is_not_mistaken_for_a_raced_choice() {
    let fixture = Fixture::start();
    fixture.choose();
    let control = fixture.control();
    control.cancel_display_switch();
    assert_eq!(control.status(), Status::Mapped);
    assert_eq!(
        refresh_display_switch(
            &control,
            DisplaySwitchMode::Controlled,
            DisplaySwitchMode::Controlled,
        ),
        Err(Error::NotReady),
    );
}

#[test]
fn window_manager_close_during_refresh_keeps_the_existing_terminal_path() {
    let fixture = Fixture::start();
    let control = fixture.control();
    let id = control.target().unwrap().window();
    assert_eq!(control.status(), Status::Mapped);
    assert!(
        Command::new("/usr/bin/python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/viewer_window/peer.py"))
            .args([&fixture.display.name, &id.to_string(), "close"])
            .status()
            .unwrap()
            .success()
    );
    let until = Instant::now() + Duration::from_secs(2);
    while control.status() != Status::CloseRequested {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    // The frd Requesting branch still refuses a successful close; this only
    // preserves the already existing local Break path, without a lease claim.
    assert_eq!(
        refresh_display_switch(
            &control,
            DisplaySwitchMode::Controlled,
            DisplaySwitchMode::Disabled,
        ),
        Ok(true),
    );
}
