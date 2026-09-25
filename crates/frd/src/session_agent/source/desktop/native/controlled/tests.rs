use super::*;
use crate::session_agent::{ApprovalMode, PlatformKind};
use fr_core::{
    input::{DesktopPoint, InputBounds},
    input_submission::Capability,
};

fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
}
fn profile(
    agent: &str,
    display: &str,
    xauthority: Option<&str>,
    fps: u16,
) -> Result<ControlProfile, InvalidLaunch> {
    ControlProfile::new(
        Path::new(agent),
        display,
        xauthority.map(Path::new),
        Seat::default(),
        caps(),
        fps,
        2_000_000,
        Backend::SoftwareExplicit,
    )
}

#[test]
fn control_profile_accepts_only_local_operator_choices() {
    assert!(profile("/usr/libexec/fr-input-agent", ":0", None, 15).is_ok());
    assert!(
        profile(
            "/usr/libexec/fr-input-agent",
            ":1.0",
            Some("/run/user/1/xauth"),
            30
        )
        .is_ok()
    );
    for (agent, display, xauthority, fps) in [
        ("fr-input-agent", ":0", None, 15),
        ("/usr/libexec/fr-input-agent", "remote:0", None, 15),
        ("/usr/libexec/fr-input-agent", ":0.1.2", None, 15),
        ("/usr/libexec/fr-input-agent", ":0", Some("xauth"), 15),
        ("/usr/libexec/fr-input-agent", ":0", None, 0),
    ] {
        assert_eq!(
            profile(agent, display, xauthority, fps).err(),
            Some(InvalidLaunch),
            "{agent} {display} {xauthority:?} {fps}"
        );
    }
    let debug = format!(
        "{:?}",
        profile("/secret/path/agent", ":7", None, 15).unwrap()
    );
    assert!(
        !debug.contains("secret") && !debug.contains(":7"),
        "{debug}"
    );
}

#[test]
fn an_agent_is_observation_only_until_a_profile_is_attached() {
    let agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::LinuxX11,
        3,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    );
    assert!(agent.control_profile().is_none());
    let seat = Seat::default();
    let agent = agent.with_control(
        ControlProfile::new(
            Path::new("/usr/libexec/fr-input-agent"),
            ":0",
            None,
            seat.clone(),
            caps(),
            15,
            2_000_000,
            Backend::SoftwareExplicit,
        )
        .unwrap(),
    );
    let attached = agent.control_profile().unwrap();
    assert_eq!(attached.capabilities(), caps());
    // The one Seat is shared, never replaced by a fresh one.
    let reservation = attached.seat().reserve().unwrap();
    assert!(seat.is_occupied());
    drop(reservation);
    assert!(!seat.is_occupied());
}

#[test]
fn served_reports_one_viewer_with_its_own_ending() {
    let ok = served(false);
    assert_eq!(
        (ok.viewers.admitted, ok.viewers.finished, ok.viewers.failed),
        (1, 1, 0)
    );
    let peer = served(true);
    assert_eq!(
        (
            peer.viewers.admitted,
            peer.viewers.finished,
            peer.viewers.failed
        ),
        (1, 0, 1)
    );
}
