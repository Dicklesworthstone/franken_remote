//! Real supervised IPC with explicit observation responses and opaque payloads.
//! These exercise the original authority owner, not live tailnet or HEVC proof.
use super::*;
use asupersync::{time::sleep_until, types::Time};
use fr_core::{authority::AuthorityError, time::HostInstant};
use fr_media::access_unit::FrameId;
use frd::worker::{Error as WorkerError, State};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

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

async fn delayed_source(control: &ObservationControl, mode: &str) -> CaptureSource {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-capture-authority-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::executable::write(
        &file,
        include_str!("../support/capture_authority_fixture.py").replace("@MODE@", mode),
    );
    CaptureSource::start(
        control,
        Launch::new(&file, ":0", None, Role::Capture, 91).unwrap(),
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

async fn near_expiry(clock: &Cx, control: &ObservationControl) -> HostInstant {
    let original = control.observation_until().unwrap();
    sleep_until(Time::from_nanos((original.as_micros() - 400_000) * 1000)).await;
    assert!(host_now(clock).unwrap() < original);
    original
}

async fn with_renewals<F: Future>(
    clock: &Cx,
    control: &ObservationControl,
    operation: F,
) -> (F::Output, u128) {
    let mut operation = pin!(operation);
    let mut timer = pin!(sleep(
        clock.timer_driver().unwrap().now(),
        Duration::from_millis(200),
    ));
    let mut responses = 0;
    let result = poll_fn(|task| {
        if let Poll::Ready(result) = operation.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if timer.as_mut().poll(task).is_ready() {
            responses += 1;
            // A simulated peer answers the actual owner's outstanding challenge.
            // Advancing a clock or merely issuing a challenge is not renewal.
            control.issue_challenge(responses).unwrap();
            control.renew(responses).unwrap();
            timer.as_mut().set(sleep(
                clock.timer_driver().unwrap().now(),
                Duration::from_millis(200),
            ));
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await;
    (result, responses)
}

async fn during_pending<F: Future>(clock: &Cx, operation: F, action: impl FnOnce()) -> F::Output {
    let mut operation = pin!(operation);
    let mut timer = pin!(sleep(
        clock.timer_driver().unwrap().now(),
        Duration::from_millis(150),
    ));
    let mut action = Some(action);
    poll_fn(|task| {
        if let Poll::Ready(result) = operation.as_mut().poll(task) {
            assert!(action.is_none(), "the original IPC must have been pending");
            return Poll::Ready(result);
        }
        if action.is_some() && timer.as_mut().poll(task).is_ready() {
            action.take().unwrap()();
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

async fn reap_failed(runtime: &Runtime, source: &mut CaptureSource) {
    // Revocation cancelled the operation's context, never this cleanup owner.
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    let worker = source.worker_mut();
    assert!(!worker.reap(&cleanup, deadline).await.unwrap().success());
    assert_eq!(worker.state(), State::Reaped);
}

#[test]
fn renewed_observation_keeps_the_original_capture_and_poll_alive_past_the_old_cap() {
    run_shared!(rt, clock, {
        for (id, mode) in [(301, "direct"), (302, "poll")] {
            let control = observation(&rt, id);
            let mut source = delayed_source(&control, mode).await;
            let original_pid = source.worker_id();
            assert!(original_pid.is_some());
            let original_until = near_expiry(&clock, &control).await;
            let before = host_now(&clock).unwrap();
            let (result, responses) =
                with_renewals(&clock, &control, source.capture_if_changed(&control, true)).await;
            let update = result.unwrap();
            assert!(responses >= 2);
            assert!(host_now(&clock).unwrap() >= original_until);
            assert!(control.observation_until().unwrap() > original_until);
            assert_eq!(source.worker_id(), original_pid);
            assert_eq!(update.frame(), FrameId::FIRST);
            assert!(update.encoded().is_some());
            assert!(update.observed_micros() >= before.as_micros());
            assert!(update.observed_micros() < original_until.as_micros());
            control.check().unwrap();
            stop(&mut source, &clock).await;
        }
    });
}

#[test]
fn repeated_poll_replies_and_renewals_cannot_restart_the_two_second_capture_ceiling() {
    run_shared!(rt, clock, {
        let control = observation(&rt, 303);
        let mut source = delayed_source(&control, "endless").await;
        near_expiry(&clock, &control).await;
        let before = host_now(&clock).unwrap();
        let (result, responses) =
            with_renewals(&clock, &control, source.capture_if_changed(&control, true)).await;
        assert_eq!(result.unwrap_err(), Error::Worker(WorkerError::Deadline));
        assert!(responses >= 5);
        assert!(host_now(&clock).unwrap().as_micros() >= before.as_micros() + 2_000_000);
        control.check().unwrap();
        let worker = source.worker_mut();
        assert_eq!(worker.state(), State::Poisoned);
        let deadline = Deadline::after(&clock, Duration::from_secs(1)).unwrap();
        assert_eq!(
            worker
                .request(&clock, Kind::Poll, vec![], deadline)
                .await
                .unwrap_err(),
            WorkerError::Unavailable
        );
        reap_failed(&rt, &mut source).await;
    });
}

#[test]
fn unanswered_challenges_and_revocation_do_not_keep_a_silent_capture_alive() {
    run_shared!(rt, clock, {
        for (id, revoke) in [(304, false), (305, true)] {
            let control = observation(&rt, id);
            let mut source = delayed_source(&control, "silent").await;
            let original_until = near_expiry(&clock, &control).await;
            let result = during_pending(&clock, source.capture_if_changed(&control, true), || {
                if revoke {
                    control.revoke();
                } else {
                    control.issue_challenge(501).unwrap();
                }
            })
            .await;
            if revoke {
                assert_eq!(result.unwrap_err(), Error::Worker(WorkerError::Cancelled));
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    Error::Authority(AuthorityError::ObservationExpired)
                );
                assert!(host_now(&clock).unwrap() >= original_until);
                assert!(control.renew(501).is_err());
            }
            assert_eq!(source.worker_mut().state(), State::Poisoned);
            reap_failed(&rt, &mut source).await;
        }
    });
}
