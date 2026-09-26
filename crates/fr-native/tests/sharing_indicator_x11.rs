//! Real XCB window and Xlib/XTest interaction. Authority/identity below are
//! explicit fixtures. This does not claim a live tailnet, WM, or human consent.
#![cfg(all(target_os = "linux", feature = "linux-session-ui"))]
#![forbid(unsafe_code)]
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, Capability, InputSession},
    time::HostDuration,
};
use fr_native::sharing_indicator::{Error, IndicatorControl, SharingIndicator, Status, StopReason};
use frd::media::{ObservationControl, host_now};
use std::{
    process::Command,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
fn display() -> Option<String> {
    if let Ok(display) = std::env::var("DISPLAY") {
        Some(display)
    } else {
        assert!(
            std::env::var_os("FR_NATIVE_INDICATOR_REQUIRED").is_none(),
            "required native indicator tests need an X11 display"
        );
        eprintln!("BLOCKED: no X11 display; no native indicator qualification");
        None
    }
}
fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn gate(r: &Runtime, lifetime: u64) -> (ObservationControl, InputSession) {
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let now = host_now(&cx).unwrap();
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(9),
        lease: InputLeaseId::from_raw(10),
        ticket: InputTicketId::from_raw(11),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(
        credentials.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(lifetime / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(credentials.lease, now).unwrap();
    a.issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    let observation = ObservationControl::new(cx, a).unwrap();
    let input = observation
        .input_session(
            credentials,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default().with(Capability::Keys),
        )
        .unwrap();
    (observation, input)
}
fn wait(control: &IndicatorControl, wanted: impl Fn(Status) -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !wanted(control.status()) {
        assert!(
            Instant::now() < until,
            "indicator did not progress: {:?}",
            control.status()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn mapped(panel: &SharingIndicator) -> IndicatorControl {
    let control = panel.control();
    wait(&control, |s| s != Status::Opening);
    assert_eq!(control.status(), Status::Mapped);
    control
}
fn finish(panel: &mut SharingIndicator) -> StopReason {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(reason) = panel.finish() {
            return reason;
        }
        assert!(Instant::now() < until, "native cleanup did not finish");
        thread::sleep(Duration::from_millis(1));
    }
}
fn peer(display: &str, control: &IndicatorControl, op: &str, args: &[&str]) {
    assert!(
        Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/sharing_indicator/peer.py"
            ))
            .arg(display)
            .arg(control.window().unwrap().to_string())
            .arg(op)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "independent X11 peer failed"
    );
}
#[test]
fn invalid_display_and_expired_authority_refuse_without_native_initialization() {
    let r = runtime();
    for display in [
        "",
        "localhost:0",
        "remote:0",
        ":",
        ":0.1.2",
        ":000000",
        ":0\0",
    ] {
        let (observation, _input) = gate(&r, 3_000_000);
        assert!(matches!(
            SharingIndicator::start(display, observation.clone()),
            Err(Error::InvalidDisplay)
        ));
        assert!(observation.check().is_err());
    }
    let (observation, _input) = gate(&r, 3_000_000);
    observation.revoke();
    assert!(matches!(
        SharingIndicator::start(":0", observation),
        Err(Error::AuthorityEnded)
    ));
}
#[test]
fn rendered_stop_button_revokes_original_input_and_observation_not_equal_id_foreign_owner() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let r = runtime();
    let (observation, input) = gate(&r, 3_000_000);
    let (foreign, _other_input) = gate(&r, 3_000_000);
    let mut panel = SharingIndicator::start(&display, observation.clone()).unwrap();
    let control = mapped(&panel);
    let snapshot = std::env::var("FR_NATIVE_INDICATOR_SNAPSHOT").ok();
    let args: Vec<_> = snapshot.as_deref().into_iter().collect();
    peer(&display, &control, "snapshot", &args);
    peer(&display, &control, "miss", &[]);
    assert_eq!(control.status(), Status::Mapped);
    assert!(observation.check().is_ok());
    assert!(!input.monitor().is_revoked());
    peer(&display, &control, "device-click", &[]);
    assert_eq!(finish(&mut panel), StopReason::User);
    assert!(observation.check().is_err());
    // Deadline consults the same closed authority, not just a separate UI flag.
    assert!(
        input
            .monitor()
            .deadline(fr_core::time::HostInstant::from_micros(0))
            .is_err()
    );
    assert!(foreign.check().is_ok());
    assert_eq!(panel.finish(), Some(StopReason::User));
}
#[test]
fn keyboard_accelerators_revoke_without_network_or_media_progress() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let r = runtime();
    for key in ["0xff1b", "0xff0d", "0x20"] {
        let (observation, _input) = gate(&r, 3_000_000);
        let mut panel = SharingIndicator::start(&display, observation.clone()).unwrap();
        let control = mapped(&panel);
        // Deliberately no session driver, socket or native media worker running.
        peer(&display, &control, "device-key", &[key]);
        assert_eq!(finish(&mut panel), StopReason::User);
        assert!(observation.check().is_err());
    }
}
#[test]
fn hidden_resized_or_destroyed_indicator_never_leaves_invisible_authority() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let r = runtime();
    for (op, expected) in [
        ("unmap", StopReason::Hidden),
        ("resize", StopReason::Hidden),
        ("cover", StopReason::Hidden),
        ("destroy", StopReason::Hidden),
    ] {
        let (observation, _input) = gate(&r, 3_000_000);
        let mut panel = SharingIndicator::start(&display, observation.clone()).unwrap();
        let control = mapped(&panel);
        peer(&display, &control, op, &[]);
        // X11 destroy unmaps a mapped window before DestroyNotify.
        assert_eq!(finish(&mut panel), expected);
        assert!(observation.check().is_err());
    }
}
#[test]
fn native_open_failure_idle_expiry_owner_drop_and_independent_stop_are_terminal() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let r = runtime();
    let (observation, _input) = gate(&r, 3_000_000);
    let mut missing = SharingIndicator::start(":59999", observation.clone()).unwrap();
    assert_eq!(finish(&mut missing), StopReason::NativeFailure);
    assert!(observation.check().is_err());
    let (observation, _input) = gate(&r, 300_000);
    let mut panel = SharingIndicator::start(&display, observation.clone()).unwrap();
    mapped(&panel);
    assert_eq!(finish(&mut panel), StopReason::AuthorityEnded);
    assert!(observation.check().is_err());
    let (observation, _input) = gate(&r, 3_000_000);
    let mut panel = SharingIndicator::start(&display, observation.clone()).unwrap();
    let control = mapped(&panel);
    control.stop();
    assert!(
        observation.check().is_err(),
        "stop must fence before returning"
    );
    assert_eq!(control.status(), Status::Stopped(StopReason::User));
    assert_eq!(finish(&mut panel), StopReason::User);
    let (observation, _input) = gate(&r, 3_000_000);
    let panel = SharingIndicator::start(&display, observation.clone()).unwrap();
    let control = panel.control();
    drop(panel);
    assert!(observation.check().is_err());
    assert_eq!(control.status(), Status::Stopped(StopReason::OwnerDropped));
    thread::sleep(Duration::from_millis(40));
    assert_eq!(control.status(), Status::Stopped(StopReason::OwnerDropped));
}

#[test]
fn native_surface_contract_retains_the_real_thread_until_cleanup() {
    use frd::local_sharing::{State, Surface};
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let r = runtime();
    let (observation, _input) = gate(&r, 3_000_000);
    let panel = SharingIndicator::start(&display, observation.clone()).unwrap();
    mapped(&panel);
    let mut surface: Box<dyn Surface> = Box::new(panel);
    assert_eq!(surface.state(), State::Ready);
    assert!(surface.original().check().is_ok());
    assert!(!surface.finish());
    surface.stop();
    assert!(observation.check().is_err());
    assert_eq!(surface.state(), State::Stopped);
    let until = Instant::now() + Duration::from_secs(2);
    while !surface.finish() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(surface.finish());
}
