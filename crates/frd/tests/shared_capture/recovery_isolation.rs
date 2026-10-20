//! Supervised child IPC, original authority and actual subscription/egress owners.
//! Opaque payloads and simulated decoder completions do not qualify HEVC/tailnet.
use super::*;
use asupersync::{time::sleep_until, types::Time};
use fr_core::{authority::AuthorityError, time::HostInstant};
use fr_media::access_unit::FrameKind;
use frd::worker::{Error as WorkerError, State};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::Poll,
};

fn physical() -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 4 * 1024 * 1024, 2).unwrap()
}

fn failed_policy() -> SendPolicy {
    SendPolicy {
        recovery_horizon_micros: 100_000,
        ..SendPolicy::default()
    }
}

fn replacement() -> MediaEpoch {
    MediaEpoch {
        recovery: epoch().recovery.next().unwrap(),
        ..epoch()
    }
}

fn observation(runtime: &Runtime, id: u128) -> ObservationControl {
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(id),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(1_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(host_now(&cx).unwrap())
        .unwrap();
    ObservationControl::new(cx, authority).unwrap()
}

async fn delayed_source(control: &ObservationControl) -> CaptureSource {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-shared-recovery-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::executable::write(&file, include_str!("../support/shared_recovery_fixture.py"));
    CaptureSource::start(
        control,
        Launch::new(&file, ":0", None, Role::Capture, 92).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: epoch().configuration,
        },
    )
    .await
    .unwrap()
}

async fn shared_capture(
    source: &mut CaptureSource,
    control: &ObservationControl,
    pool: &SharedFramePool,
) -> Result<SharedCaptureUpdate, Error> {
    source
        .prepare_shared_capture(control, pool)?
        .capture_if_changed(false)
        .await
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|task| match future.as_mut().poll(task) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("the original supervised capture must be pending"),
    })
    .await;
}

fn request(sender: &mut Egress, source: &mut CaptureSource) -> HostInstant {
    let binding = Binding {
        parent: ControlBinding {
            id: 10,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(2),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: epoch().configuration,
        recovery: epoch().recovery,
        viewport: ViewportMappingGeneration::INITIAL,
    };
    let mut bytes = [0; recovery_request::REQUEST_BYTES];
    recovery_request::encode(
        Request {
            reason: Reason::ReferenceExpired,
            last_useful_frame: Some(0),
        },
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert!(sender.request_recovery(source, &bytes, binding).unwrap());
    sender.next_deadline().unwrap()
}

async fn reap_poisoned(runtime: &Runtime, source: &mut CaptureSource) {
    // Cancellation/expiry of the observation cannot cancel this cleanup owner.
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    let worker = source.worker_mut();
    assert_eq!(worker.state(), State::Poisoned);
    assert_eq!(
        worker
            .request(&cleanup, Kind::Poll, vec![], deadline)
            .await
            .unwrap_err(),
        WorkerError::Unavailable
    );
    assert!(!worker.reap(&cleanup, deadline).await.unwrap().success());
    assert_eq!(worker.state(), State::Reaped);
}

#[test]
fn expired_viewer_cannot_kill_shared_capture_needed_by_the_healthy_original_subscription() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (failed, failed_input) = gate(&rt, 2);
        let (healthy, healthy_input) = gate(&rt, 3);
        let mut source = delayed_source(&owner).await;
        let pid = source.worker_id();
        assert!(pid.is_some());
        let pool = physical();
        let mut a = egress(&failed, limits(1150), bindings(1), failed_policy());
        let mut b = egress(&healthy, limits(1150), bindings(11), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(1150), bindings(11));
        let initial = shared_capture(&mut source, &owner, &pool).await.unwrap();
        assert_eq!(
            initial.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Ok(()), Ok(())]
        );
        drop(initial);
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        assert_eq!(decoded(&cx, &mut ar).descriptor().frame, 0);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 0);

        let failed_until = request(&mut a, &mut source);
        assert!(!input_live(&cx, &failed_input));
        assert!(input_live(&cx, &healthy_input));
        assert_eq!(pool.usage().pictures, 1);
        let update = {
            let mut capture = pin!(shared_capture(&mut source, &owner, &pool));
            pending(capture.as_mut()).await;
            // Prove the native request began while this demand was live. A
            // scheduler simply discarding an already-expired request cannot pass.
            assert!(host_now(&cx).unwrap() < failed_until);
            assert_eq!(pool.usage().pictures, 2);
            assert!(pool.usage().bytes > 1024 * 1024);
            capture.await.unwrap()
        };
        assert!(host_now(&cx).unwrap() >= failed_until);
        assert_eq!(a.next_deadline(), Some(failed_until));
        assert_eq!(source.next_shared_recovery_deadline(), None);
        assert_eq!(source.worker_id(), pid);
        assert_eq!(update.frame().as_raw(), 1);
        assert!(matches!(
            update.encoded().unwrap().kind(),
            FrameKind::Idr { .. }
        ));
        // Conversion refunds unused native capacity while retaining two actual
        // pictures: the healthy viewer's bootstrap and this new shared IDR.
        assert_eq!(pool.usage().pictures, 2);
        assert!(pool.usage().bytes < 16_384);

        assert_eq!(
            a.recover(replacement(), bindings(21)).unwrap_err(),
            Error::Send(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert_eq!(
            update.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Err(Error::Send(SendError::Closed)), Ok(())]
        );
        drop(update);
        let sent = pump(&cx, &mut b, &mut br);
        assert!(sent.contains(&Channel::Video));
        assert!(!sent.contains(&Channel::Recovery));
        let decoded = decoded(&cx, &mut br);
        assert_eq!(decoded.descriptor().frame, 1);
        assert_eq!(decoded.epoch(), epoch());
        assert!(!input_live(&cx, &failed_input));
        assert!(input_live(&cx, &healthy_input));
        owner.check().unwrap();
        assert_eq!(a.cache_usage(), BudgetUsage::default());
        a.close();
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert_eq!(source.worker_id(), pid);
        assert_eq!(source.worker_mut().state(), State::Running);
        stop(&mut source, &cx).await;
    });
}

#[test]
fn private_capture_keeps_the_original_recovery_timeout_and_requires_independent_reaping() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (viewer, input) = gate(&rt, 2);
        let mut source = delayed_source(&owner).await;
        let pid = source.worker_id();
        let pool = physical();
        let mut sender = egress(&viewer, limits(1150), bindings(1), failed_policy());
        let mut received = receiver(&cx, limits(1150), bindings(1));
        let initial = shared_capture(&mut source, &owner, &pool).await.unwrap();
        sender.enqueue_shared_capture(&initial).unwrap();
        drop(initial);
        pump(&cx, &mut sender, &mut received);
        assert_eq!(decoded(&cx, &mut received).descriptor().frame, 0);
        let failed_until = request(&mut sender, &mut source);
        assert_eq!(pool.usage(), BudgetUsage::default());
        let result = {
            let mut capture = pin!(source.capture_if_changed(&owner, false));
            pending(capture.as_mut()).await;
            assert!(host_now(&cx).unwrap() < failed_until);
            capture.await
        };
        assert_eq!(result.unwrap_err(), Error::Worker(WorkerError::Deadline));
        assert!(host_now(&cx).unwrap() >= failed_until);
        assert_eq!(sender.next_deadline(), Some(failed_until));
        assert_eq!(source.worker_id(), pid);
        assert_eq!(
            sender.recover(replacement(), bindings(21)).unwrap_err(),
            Error::Send(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert!(!input_live(&cx, &input));
        owner.check().unwrap();
        reap_poisoned(&rt, &mut source).await;
    });
}

#[test]
fn shared_capture_still_fences_source_observation_expiry_and_revoke_while_pending() {
    run_shared!(rt, cx, {
        for (id, revoke) in [(401, false), (402, true)] {
            let control = observation(&rt, id);
            let mut source = delayed_source(&control).await;
            let pid = source.worker_id();
            let pool = physical();
            drop(shared_capture(&mut source, &control, &pool).await.unwrap());
            let until = control.observation_until().unwrap();
            sleep_until(Time::from_nanos((until.as_micros() - 100_000) * 1000)).await;
            let before = host_now(&cx).unwrap();
            let result = {
                let mut capture = pin!(shared_capture(&mut source, &control, &pool));
                pending(capture.as_mut()).await;
                assert!(host_now(&cx).unwrap() < until);
                assert_eq!(pool.usage().pictures, 1);
                if revoke {
                    control.revoke();
                } else {
                    // Issuing an unanswered challenge must not renew authority.
                    control.issue_challenge(601).unwrap();
                }
                capture.await
            };
            if revoke {
                assert_eq!(result.unwrap_err(), Error::Worker(WorkerError::Cancelled));
            } else {
                // The second worker clock sample may cross the same authority
                // bound immediately after its callback accepted the first one.
                assert!(matches!(
                    result.unwrap_err(),
                    Error::Authority(AuthorityError::ObservationExpired)
                        | Error::Worker(WorkerError::Deadline)
                ));
                assert_eq!(
                    control.check().unwrap_err(),
                    Error::Authority(AuthorityError::ObservationExpired)
                );
                let finished = host_now(&cx).unwrap();
                assert!(finished >= until);
                assert!(finished.as_micros() < before.as_micros() + 2_000_000);
                assert!(control.renew(601).is_err());
            }
            assert_eq!(pool.usage(), BudgetUsage::default());
            assert_eq!(source.worker_id(), pid);
            reap_poisoned(&rt, &mut source).await;
        }
    });
}
