//! Real child IPC and authority; opaque test units do not qualify HEVC.
use super::*;
use crate::worker::{Deadline, Launch};
use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep};
use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
use fr_media::{
    delivery::{MediaBindings, MediaEpoch, SendPolicy},
    worker::{Backend, Configuration, Role},
};
use fr_wire::{
    MediaLimits,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    recovery_request::{self, Reason, Request},
};
use std::{
    future::{Future, poll_fn},
    os::unix::fs::PermissionsExt,
    task::Poll,
    time::Duration,
};
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
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn request(frame: Option<u64>) -> Vec<u8> {
    let mut out = vec![0; recovery_request::REQUEST_BYTES];
    recovery_request::encode(
        Request {
            reason: Reason::ReferenceExpired,
            last_useful_frame: frame,
        },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut out,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    out
}
async fn source(control: &super::super::ObservationControl) -> CaptureSource {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-deferred-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let script=include_str!("../../../tests/support/recovery_worker_fixture.py").replace("@MODE@","healthy").replace("    if kind == 7 and not force", "    if kind == 7 and not force and last is not None:\n        time.sleep(0.1)\n    if kind == 7 and not force");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, Role::Capture, 77).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: binding().configuration,
        },
    )
    .await
    .unwrap()
}
async fn fixture(
    cx: &Cx,
    budget: u64,
) -> (
    super::super::ObservationControl,
    CaptureSource,
    Subscription,
) {
    let mut a = SessionAuthority::new(
        binding().parent.remote_session,
        AuthorityPolicy::plan_defaults(),
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(host_now(cx).unwrap()).unwrap();
    a.mark_view_ready(host_now(cx).unwrap()).unwrap();
    let control = super::super::ObservationControl::new(cx.clone(), a).unwrap();
    let mut source = source(&control).await;
    let mut sub = Subscription::new(
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
    sub.enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
        .unwrap();
    (control, source, sub)
}
async fn reap(source: &mut CaptureSource, cx: &Cx) {
    source.worker.abort();
    source
        .worker
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}
#[test]
fn failure_admission_during_native_borrow_fences_packets_before_capture_returns() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, mut source, mut sub) = fixture(&cx, 2_000_000).await;
        let packet = sub.next_packet(&mut [0; 1150]).unwrap().unwrap();
        let mut native = Box::pin(source.capture_if_changed(&control, false));
        poll_fn(|task| {
            assert!(native.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        let pending = sub
            .admit_recovery_request(&request(Some(0)), binding())
            .unwrap()
            .unwrap();
        assert!(!control.view_ready().unwrap());
        assert!(sub.authorize_write(&packet).is_err());
        let until = pending.deadline_micros();
        assert!(
            sub.admit_recovery_request(&request(Some(0)), binding())
                .unwrap()
                .is_none()
        );
        assert_eq!(sub.recovery_deadline().unwrap(), until);
        let obsolete = native.await.unwrap();
        assert!(obsolete.is_unchanged());
        drop(obsolete);
        sub.schedule_recovery(&mut source, pending).unwrap();
        assert!(
            source
                .capture_if_changed(&control, false)
                .await
                .unwrap()
                .encoded()
                .unwrap()
                .is_idr()
        );
        assert!(!control.view_ready().unwrap());
        reap(&mut source, &cx).await;
    });
}
#[test]
fn a_different_source_cannot_consume_a_deferred_demand() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, mut original, mut sub) = fixture(&cx, 2_000_000).await;
        let mut foreign = source(&control).await;
        assert!(
            sub.request_recovery(&mut foreign, &request(Some(0)), binding())
                .is_err()
        );
        assert!(control.view_ready().unwrap());
        let pending = sub
            .admit_recovery_request(&request(Some(0)), binding())
            .unwrap()
            .unwrap();
        assert!(sub.schedule_recovery(&mut foreign, pending).is_err());
        assert!(foreign.next_recovery_deadline().is_none());
        reap(&mut original, &cx).await;
        reap(&mut foreign, &cx).await;
    });
}
#[test]
fn draining_time_cannot_renew_a_pending_capture_recovery_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, mut source, mut sub) = fixture(&cx, 100_000).await;
        let pending = sub
            .admit_recovery_request(&request(Some(0)), binding())
            .unwrap()
            .unwrap();
        sleep(cx.now(), Duration::from_millis(150)).await;
        assert!(matches!(
            sub.schedule_recovery(&mut source, pending),
            Err(Error::Receiver(DeliveryError::RecoveryExpired))
        ));
        assert!(source.next_recovery_deadline().is_none());
        assert!(control.check().is_ok());
        reap(&mut source, &cx).await;
    });
}
#[test]
fn raw_worker_access_invalidates_the_deferred_source_identity() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (_, mut source, mut sub) = fixture(&cx, 2_000_000).await;
        let pending = sub
            .admit_recovery_request(&request(Some(0)), binding())
            .unwrap()
            .unwrap();
        let _ = source.worker_mut();
        assert!(sub.schedule_recovery(&mut source, pending).is_err());
        assert!(source.next_recovery_deadline().is_none());
        reap(&mut source, &cx).await;
    });
}
