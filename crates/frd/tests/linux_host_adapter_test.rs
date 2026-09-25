//! Tests for the Linux host adapters that remain after the Wayland portal,
//! `PipeWire` and EIS models were removed (2026-09-24): the X11 adapter's
//! security diagnostics and input pairing, and systemd graphical-session
//! environment validation.

use std::path::PathBuf;

use fr_core::{
    input::{DesktopPoint, InputBounds, PointerButton},
    input_submission::{InputSink, Operation, PlatformError, Submission},
};

use frd::linux::{
    GraphicalSessionEnvironment, PlatformSecurityModel, SessionEnvironmentError, SessionType,
    X11EventPoster, X11InputSink,
};

/// Test-only poster (moved here from the library): it accepts every event and
/// injects nothing. It drives the sink's pairing and bounds logic; it is not
/// X11 injection evidence.
struct AcceptingTestPoster;

impl X11EventPoster for AcceptingTestPoster {
    fn fake_motion_event(&mut self, _x: i32, _y: i32) -> Result<(), PlatformError> {
        Ok(())
    }

    fn fake_button_event(&mut self, _button: u32, _down: bool) -> Result<(), PlatformError> {
        Ok(())
    }

    fn fake_key_event(&mut self, _keycode: u32, _down: bool) -> Result<(), PlatformError> {
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }
}

fn make_test_bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds")
}

#[test]
fn test_x11_host_adapter_security_diagnostics_and_pairs() {
    let mut x11_sink = X11InputSink::new(AcceptingTestPoster, make_test_bounds());

    // Surfaced security diagnostics
    let diag = x11_sink.security_report();
    assert_eq!(diag.model, PlatformSecurityModel::X11Unconfined);
    assert!(!diag.window_isolation);
    assert!(!diag.consent_prompt_enforced);
    assert!(diag.diagnostic_warning.is_some());

    // Pair requirements for X11
    assert!(x11_sink.repeat_requires_pair());
    assert!(x11_sink.line_scroll_requires_pairs());

    // Pointer injection works
    let pt_op = Operation::Absolute(DesktopPoint { x: 200, y: 300 });
    assert_eq!(x11_sink.prepare(pt_op), Ok(()));
    assert_eq!(x11_sink.submit(pt_op), Submission::Submitted);

    // Button injection works
    let btn_op = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };
    assert_eq!(x11_sink.prepare(btn_op), Ok(()));
    assert_eq!(x11_sink.submit(btn_op), Submission::Submitted);
}

#[test]
fn test_systemd_session_environment_detection() {
    let mut env = GraphicalSessionEnvironment {
        session_type: SessionType::Wayland,
        wayland_display: Some("wayland-0".into()),
        x11_display: None,
        dbus_session_bus_address: Some("unix:path=/run/user/1000/bus".into()),
        current_desktop: Some("GNOME".into()),
        user_runtime_dir: Some(PathBuf::from("/run/user/1000")),
    };

    // Valid Wayland hosting environment
    assert!(env.validate_for_hosting().is_ok());

    // Missing WAYLAND_DISPLAY in Wayland session triggers typed refusal
    env.wayland_display = None;
    assert_eq!(
        env.validate_for_hosting(),
        Err(SessionEnvironmentError::WaylandDisplayMissing)
    );

    // Missing D-Bus session bus triggers typed refusal
    env.wayland_display = Some("wayland-0".into());
    env.dbus_session_bus_address = None;
    env.user_runtime_dir = Some(PathBuf::from("/nonexistent/dir"));
    assert_eq!(
        env.validate_for_hosting(),
        Err(SessionEnvironmentError::DbusSessionBusMissing)
    );
}
