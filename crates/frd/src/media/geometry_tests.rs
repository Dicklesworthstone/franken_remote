//! Real supervised child IPC; the monitor/capture peer is an explicit fixture,
//! not evidence of X11, HEVC or a live monitor resize.
use super::*;
use crate::input_watchdog::{StopReason, Watchdog};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::AuthorityPolicy,
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::Capabilities,
    time::HostDuration,
};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn owner(rt: &Runtime, id: u128) -> ObservationControl {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(id),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(host_now(&cx).unwrap())
        .unwrap();
    ObservationControl::new(cx, authority).unwrap()
}
async fn source(control: &ObservationControl, mode: &str) -> (CaptureSource, worker::Retirement) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-geometry-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::executable::write(
        &path,
        include_str!("../../tests/support/local_source_fixture.py").replace("@MODE@", mode),
    );
    let (launch, retirement) = Launch::new(&path, ":0", None, Role::Capture, 71)
        .unwrap()
        .retain_cleanup()
        .unwrap();
    let discovery = discovery::DiscoveredSource::start(control, launch)
        .await
        .unwrap();
    let catalog = discovery.catalog().unwrap();
    let source = discovery
        .configure_local(
            catalog.selection(catalog.displays()[0].handle).unwrap(),
            Configuration {
                width: 320,
                height: 240,
                fps: 30,
                backend: fr_media::worker::Backend::SoftwareExplicit,
                bitrate: 2_000_000,
                max_access_unit_bytes: 1024 * 1024,
                generation: CodecConfigurationGeneration::INITIAL,
            },
        )
        .unwrap()
        .await
        .unwrap();
    (source, retirement)
}
fn granted_watchdog(
    control: &ObservationControl,
) -> (fr_core::input_submission::InputSession, Watchdog) {
    let now = control.check().unwrap();
    let credentials = InputCredentials {
        session: control.authority.lock().unwrap().session(),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    {
        let mut authority = control.authority.lock().unwrap();
        authority.mark_view_ready(now).unwrap();
        authority.grant_lease(credentials.lease, now).unwrap();
        authority
            .issue_input_ticket(credentials.lease, credentials.ticket, now)
            .unwrap();
    }
    let input = control
        .input_session(
            credentials,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
        )
        .unwrap();
    let watchdog = Watchdog::new(control.context(), input.monitor()).unwrap();
    control.register_lease(watchdog.control());
    (input, watchdog)
}
async fn reap(retirement: &mut worker::Retirement) {
    let cleanup = Cx::current().unwrap();
    assert!(
        retirement
            .reap(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[test]
fn checked_capture_resize_fences_input_before_return_and_requires_a_fresh_catalog() {
    let rt = runtime();
    rt.block_on(async {
        let control = owner(&rt, 1);
        let signal = Arc::new(AtomicU8::new(0));
        control.retain_closure_reason(signal.clone()).unwrap();
        let (mut original, mut retirement) = source(&control, "capture-geometry").await;
        let catalog = original.selected_catalog(&control).unwrap();
        original.capture_if_changed(&control, false).await.unwrap();
        let (input, watchdog) = granted_watchdog(&control);
        let lease = watchdog.control();

        let error = original
            .capture_if_changed(&control, false)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error,
            Error::Worker(worker::Error::WorkerRefused(
                fr_media::worker::Error::GeometryChanged,
            ))
        );
        assert!(lease.is_stopped());
        assert_eq!(lease.reason(), Some(StopReason::GeometryChanged));
        assert_eq!(control.end_reason(), Some(StopReason::GeometryChanged));
        assert_eq!(
            signal.load(Ordering::Acquire),
            fr_wire::closure::ClosedReason::GeometryChanged as u8
        );
        assert!(original.selected_catalog(&control).is_err());
        assert!(original.capture_if_changed(&control, false).await.is_err());
        drop((original, watchdog, input));
        reap(&mut retirement).await;

        // A new native child and authority may observe the same physical
        // display only through a new catalog. No control grant is inherited.
        let fresh = owner(&rt, 2);
        let (mut replacement, mut retirement) = source(&fresh, "normal").await;
        let next = replacement.selected_catalog(&fresh).unwrap();
        assert_ne!(next.revision(), catalog.revision());
        assert_ne!(next.displays()[0].handle, catalog.displays()[0].handle);
        assert!(!fresh.view_ready().unwrap());
        assert!(fresh.check_control().is_err());
        assert!(replacement.capture_if_changed(&fresh, true).await.is_ok());
        assert!(
            replacement
                .capture_if_changed(&control, true)
                .await
                .is_err()
        );
        drop(replacement);
        reap(&mut retirement).await;
    });
}

#[test]
fn malformed_worker_error_or_ended_authority_never_becomes_a_geometry_close() {
    let rt = runtime();
    for error in [
        worker::Error::Protocol(fr_media::worker::Error::GeometryChanged),
        worker::Error::WorkerRefused(fr_media::worker::Error::Malformed),
        worker::Error::PeerClosed,
    ] {
        let control = owner(&rt, 1);
        let signal = Arc::new(AtomicU8::new(0));
        control.retain_closure_reason(signal.clone()).unwrap();
        assert_eq!(control.capture_failure(error), Error::Worker(error));
        assert_eq!(control.end_reason(), None);
        assert_eq!(signal.load(Ordering::Acquire), 0);
        assert!(control.check().is_ok());
    }
    let control = owner(&rt, 2);
    let signal = Arc::new(AtomicU8::new(0));
    control.retain_closure_reason(signal.clone()).unwrap();
    control.note_end(StopReason::AuthorityEnded);
    control.revoke();
    let error = control.capture_failure(worker::Error::WorkerRefused(
        fr_media::worker::Error::GeometryChanged,
    ));
    assert!(!error.is_geometry_changed());
    assert_eq!(control.end_reason(), Some(StopReason::AuthorityEnded));
    assert_eq!(signal.load(Ordering::Acquire), 0);
}

#[test]
fn an_existing_terminal_cause_survives_a_later_checked_geometry_failure() {
    let rt = runtime();
    for previous in [
        StopReason::LocalRevoke,
        StopReason::NativeFailure,
        StopReason::AuthorityEnded,
    ] {
        let control = owner(&rt, 1);
        let signal = Arc::new(AtomicU8::new(0));
        control.retain_closure_reason(signal.clone()).unwrap();
        control.note_end(previous);
        control.note_geometry_changed().unwrap();
        assert_eq!(control.end_reason(), Some(previous));
        assert_eq!(signal.load(Ordering::Acquire), 0);
    }
    for previous in [
        fr_wire::closure::ClosedReason::ClientRequested,
        fr_wire::closure::ClosedReason::ProtocolError,
        fr_wire::closure::ClosedReason::HostFailure,
    ] {
        let control = owner(&rt, 1);
        let signal = Arc::new(AtomicU8::new(previous as u8));
        control.retain_closure_reason(signal.clone()).unwrap();
        control.note_geometry_changed().unwrap();
        assert_eq!(signal.load(Ordering::Acquire), previous as u8);
    }
}
