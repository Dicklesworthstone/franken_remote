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
#[test]
fn a_file_worker_requires_observed_join_and_successful_disk_cleanup() {
    use crate::session_startup::{FileReceiveCleanup as Cleanup, FileReceiveError as E};
    use fr_files::{session::Error as SessionError, worker::Error as WorkerError};
    // Normal permission/lease stops are separated from actual cleanup by the
    // disk owner. This adapter must not turn any remaining error into success.
    for joined in [Ok(Cleanup::NotStarted), Ok(Cleanup::Finished(Ok(())))] {
        assert_eq!(files_cleanup(joined), Ok(()));
    }
    for (kept, error) in [
        (
            Ok(Cleanup::Finished(Err(WorkerError::Cleanup))),
            crate::worker::Error::Unavailable,
        ),
        (
            Ok(Cleanup::Finished(Err(WorkerError::Session(
                SessionError::Permission,
            )))),
            crate::worker::Error::Unavailable,
        ),
        (
            Ok(Cleanup::Finished(Err(WorkerError::Session(
                SessionError::Closed,
            )))),
            crate::worker::Error::Unavailable,
        ),
        (
            Ok(Cleanup::Finished(Err(WorkerError::Poisoned))),
            crate::worker::Error::Unavailable,
        ),
        (
            Ok(Cleanup::Finished(Err(WorkerError::Panicked))),
            crate::worker::Error::Unavailable,
        ),
        (Ok(Cleanup::Pending), crate::worker::Error::Deadline),
        (Err(E::CleanupPending), crate::worker::Error::Deadline),
        (Err(E::Cancelled), crate::worker::Error::Cancelled),
        (Err(E::Clock), crate::worker::Error::ClockRegression),
    ] {
        assert_eq!(files_cleanup(kept), Err(error));
    }
    // The drop directory is a separate operator choice on the profile.
    let dir = std::env::temp_dir().join(format!("fr-profile-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let base = profile("/usr/libexec/fr-input-agent", ":0", None, 30).unwrap();
    assert!(!base.files());
    let with = base.with_files(
        crate::native_files::Directory::open(&dir, crate::native_files::Limits::default()).unwrap(),
    );
    assert!(with.files() && !with.clipboard());
    assert!(!format!("{with:?}").contains("fr-profile-files"));
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn adaptive_capture_preserves_the_original_control_profile_and_seat() {
    let seat = Seat::default();
    let profile = ControlProfile::new(
        Path::new("/usr/libexec/fr-input-agent"),
        ":0",
        None,
        seat.clone(),
        caps(),
        30,
        2_000_000,
        Backend::SoftwareExplicit,
    )
    .unwrap();
    assert!(!profile.adaptive_capture());
    let display = fr_wire::display::Display {
        handle: 1,
        geometry: fr_core::ids::DisplayGeometryGeneration::INITIAL,
        x: 0,
        y: 0,
        pixel_width: 640,
        pixel_height: 480,
        logical_width: 640,
        logical_height: 480,
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
    };
    let configuration = profile.configuration(display);
    let adaptive = profile.with_adaptive_capture();
    assert!(adaptive.adaptive_capture());
    assert_eq!(adaptive.configuration(display), configuration);
    assert_eq!(adaptive.capabilities(), caps());
    assert!(!adaptive.clipboard() && !adaptive.files() && !adaptive.file_download());
    let reservation = adaptive.seat().reserve().unwrap();
    assert!(seat.is_occupied());
    drop(reservation);
    assert!(!seat.is_occupied());
}
