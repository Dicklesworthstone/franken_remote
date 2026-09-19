//! Production authority, capture owner, IPC and packetization with a test-only
//! child process. This does not qualify HEVC, platform capture or transport.
#![cfg(target_os = "linux")]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    delivery::{DeliveryError, MediaBindings, MediaEpoch, SendError, SendPolicy},
    worker::{Backend, Configuration, Kind, Role},
};
use fr_wire::{
    Channel, MediaLimits,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    recovery_request::{self, REQUEST_BYTES, Reason, Request},
};
use frd::{
    media::{CaptureSource, Error, ObservationControl, Subscription, host_now},
    worker::{Deadline, Launch, State},
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 10,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(9),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn control(cx: &Cx, id: u128) -> (ObservationControl, InputSession) {
    let now = host_now(cx).unwrap();
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(id),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(4_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(id),
        lease: InputLeaseId::from_raw(1),
        ticket: InputTicketId::from_raw(1),
        view: InputView {
            geometry: binding().geometry,
            viewport: binding().viewport,
            configuration: binding().configuration,
            recovery: binding().recovery,
        },
    };
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    let input = control
        .input_session(
            credentials,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
        )
        .unwrap();
    (control, input)
}
fn fixture(mode: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-recovery-{}-{}-{mode}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("support/recovery_worker_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
async fn source(control: &ObservationControl, mode: &str) -> CaptureSource {
    CaptureSource::start(
        control,
        Launch::new(&fixture(mode), ":0", None, Role::Capture, 77).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: CodecConfigurationGeneration::INITIAL,
        },
    )
    .await
    .unwrap()
}
async fn subscription(
    control: &ObservationControl,
    source: &mut CaptureSource,
    budget: u64,
) -> Subscription {
    let mut subscription = Subscription::new(
        control.clone(),
        MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        MediaEpoch {
            configuration: binding().configuration,
            recovery: binding().recovery,
        },
        SendPolicy {
            recovery_horizon_micros: budget,
            ..SendPolicy::default()
        },
    )
    .unwrap();
    subscription
        .enqueue_capture(source.capture_if_changed(control, true).await.unwrap())
        .unwrap();
    subscription
}
fn request(b: Binding) -> [u8; REQUEST_BYTES] {
    let mut bytes = [0; REQUEST_BYTES];
    recovery_request::encode(
        Request {
            reason: Reason::ReferenceExpired,
            last_useful_frame: Some(0),
        },
        b,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn stop(source: &mut CaptureSource, cx: &Cx) {
    let worker = source.worker_mut();
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    if worker.state() == State::Running {
        worker
            .request(cx, Kind::Stop, vec![], deadline)
            .await
            .unwrap();
    }
    worker.reap(cx, deadline).await.unwrap();
}
#[test]
fn accepted_request_fences_actual_input_and_the_existing_capture_loop_forces_recovery() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let (healthy, healthy_input) = self::control(&cx, 10);
        let mut source = source(&control, "healthy").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        let mut bytes = [0; 1150];
        let old = subscription.next_packet(&mut bytes).unwrap().unwrap();
        let worker_id = source.worker_id();
        assert!(
            subscription
                .request_recovery(&mut source, &request(binding()), binding())
                .unwrap()
        );
        let original = subscription.next_deadline();
        assert!(
            !subscription
                .request_recovery(&mut source, &request(binding()), binding())
                .unwrap()
        );
        assert_eq!(subscription.next_deadline(), original);
        assert!(subscription.authorize_write(&old).is_err());
        assert!(!control.view_ready().unwrap());
        assert!(
            input
                .monitor()
                .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
                .is_err()
        );
        assert!(healthy.view_ready().unwrap());
        healthy_input
            .monitor()
            .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
            .unwrap();
        // No new explicit force flag: the ordinary capture loop consumes the
        // original source-owned demand. Static pixels cannot return Unchanged.
        let update = source.capture_if_changed(&control, false).await.unwrap();
        assert!(update.encoded().unwrap().is_idr());
        assert_eq!(source.worker_id(), worker_id);
        assert_eq!(source.next_recovery_deadline(), None);
        let epoch = MediaEpoch {
            configuration: binding().configuration,
            recovery: binding().recovery.next().unwrap(),
        };
        subscription
            .recover(epoch, MediaBindings::new(11, 12, 13, 14).unwrap())
            .unwrap();
        subscription.enqueue_capture(update).unwrap();
        let progress = subscription.next_packet(&mut bytes).unwrap().unwrap();
        assert_eq!(progress.channel(), Channel::MediaConfig);
        let picture = subscription.next_packet(&mut bytes).unwrap().unwrap();
        assert_eq!(picture.channel(), Channel::Recovery);
        assert!(!control.view_ready().unwrap());
        assert!(
            input
                .monitor()
                .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
                .is_err()
        );
        stop(&mut source, &cx).await;
    });
}
#[test]
fn malformed_foreign_source_and_foreign_session_requests_do_not_stale_a_healthy_view() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "healthy").await;
        let mut foreign = self::source(&control, "healthy").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        assert!(
            subscription
                .request_recovery(&mut source, b"bad", binding())
                .is_err()
        );
        assert!(
            subscription
                .request_recovery(&mut foreign, &request(binding()), binding())
                .is_err()
        );
        let different = Binding {
            parent: ControlBinding {
                remote_session: RemoteSessionId::from_raw(8),
                ..binding().parent
            },
            ..binding()
        };
        assert!(
            subscription
                .request_recovery(&mut source, &request(different), different)
                .is_err()
        );
        assert!(control.view_ready().unwrap());
        input
            .monitor()
            .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
            .unwrap();
        assert_eq!(source.next_recovery_deadline(), None);
        assert_eq!(foreign.next_recovery_deadline(), None);
        assert!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap()
                .is_unchanged()
        );
        stop(&mut source, &cx).await;
        stop(&mut foreign, &cx).await;
    });
}
#[test]
fn worker_polling_and_delayed_capture_cannot_restart_the_original_recovery_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "poll-forever").await;
        let mut subscription = subscription(&control, &mut source, 400_000).await;
        subscription
            .request_recovery(&mut source, &request(binding()), binding())
            .unwrap();
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(250)).await;
        let started = std::time::Instant::now();
        assert_eq!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap_err(),
            Error::Worker(frd::worker::Error::Deadline)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(source.worker_mut().state(), State::Poisoned);
        assert!(
            input
                .monitor()
                .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
                .is_err()
        );
        assert_eq!(
            subscription.tick(),
            Err(Error::Send(SendError::Delivery(
                DeliveryError::RecoveryExpired
            )))
        );
        control.check().unwrap();
        stop(&mut source, &cx).await;
    });
}
#[test]
fn a_worker_ignoring_force_idr_is_refused_not_used_as_a_recovery_picture() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, _) = control(&cx, 9);
        let mut source = source(&control, "ignore-force").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        subscription
            .request_recovery(&mut source, &request(binding()), binding())
            .unwrap();
        assert_eq!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap_err(),
            Error::InvalidFrame
        );
        assert_eq!(source.worker_mut().state(), State::Poisoned);
        assert!(!control.view_ready().unwrap());
        stop(&mut source, &cx).await;
    });
}
#[test]
fn an_expired_failed_viewer_does_not_reset_the_shared_capture_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, _) = control(&cx, 9);
        let mut source = source(&control, "healthy").await;
        let mut subscription = subscription(&control, &mut source, 100_000).await;
        subscription
            .request_recovery(&mut source, &request(binding()), binding())
            .unwrap();
        let worker_id = source.worker_id();
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(150)).await;
        assert!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap()
                .is_unchanged()
        );
        assert_eq!(source.worker_id(), worker_id);
        assert_eq!(source.next_recovery_deadline(), None);
        assert_eq!(
            subscription.tick(),
            Err(Error::Send(SendError::Delivery(
                DeliveryError::RecoveryExpired
            )))
        );
        assert!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap()
                .is_unchanged()
        );
        stop(&mut source, &cx).await;
    });
}
