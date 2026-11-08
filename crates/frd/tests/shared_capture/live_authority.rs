//! Real supervised IPC with explicit observation responses and opaque payloads.
//! These exercise the original authority owner, not live tailnet or HEVC proof.
use super::*;
#[path = "../support/worker_pool.rs"]
mod worker_pool;
use asupersync::{time::sleep_until, types::Time};
use fr_core::{authority::AuthorityError, time::HostInstant};
use fr_media::access_unit::FrameId;
use frd::{
    media::discovery::DiscoveredSource,
    worker::{Error as WorkerError, State},
};
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

fn fixture(name: &str, source: &str, mode: &str) -> std::path::PathBuf {
    image(
        &format!("fr-{name}-authority"),
        source.replace("@MODE@", mode),
    )
}

async fn start_source(control: &ObservationControl, mode: &str) -> Result<CaptureSource, Error> {
    let file = fixture(
        "capture",
        include_str!("../support/capture_authority_fixture.py"),
        mode,
    );
    CaptureSource::start(
        control,
        Launch::new(&file, ":0", None, Role::Capture, 91).unwrap(),
        configuration(),
    )
    .await
}

async fn delayed_source(control: &ObservationControl, mode: &str) -> CaptureSource {
    start_source(control, mode).await.unwrap()
}

async fn discover(control: &ObservationControl, mode: &str) -> Result<DiscoveredSource, Error> {
    let file = fixture(
        "discovery",
        include_str!("../support/local_source_fixture.py"),
        mode,
    );
    DiscoveredSource::start(
        control,
        Launch::new(&file, ":0", None, Role::Capture, 92).unwrap(),
    )
    .await
}

fn refused<T>(result: Result<T, Error>) -> Error {
    result
        .err()
        .expect("the start must not outlive its authority")
}

fn expired(error: Error) -> bool {
    // Expiry may fall between the authority callback and the worker's second
    // clock sample. Both paths fence the same bound.
    matches!(
        error,
        Error::Authority(AuthorityError::ObservationExpired) | Error::Worker(WorkerError::Deadline)
    )
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

async fn during_pending<F: Future>(
    clock: &Cx,
    delay: Duration,
    operation: F,
    action: impl FnOnce(),
) -> F::Output {
    let mut operation = pin!(operation);
    let mut timer = pin!(sleep(clock.timer_driver().unwrap().now(), delay));
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
            let before = host_now(&clock).unwrap();
            let result = during_pending(
                &clock,
                Duration::from_millis(150),
                source.capture_if_changed(&control, true),
                || {
                    if revoke {
                        control.revoke();
                    } else {
                        control.issue_challenge(501).unwrap();
                    }
                },
            )
            .await;
            if revoke {
                assert_eq!(result.unwrap_err(), Error::Worker(WorkerError::Cancelled));
            } else {
                // Expiry may fall between the authority callback and the
                // worker's second clock sample. Both paths fence the same bound.
                assert!(matches!(
                    result.unwrap_err(),
                    Error::Authority(AuthorityError::ObservationExpired)
                        | Error::Worker(WorkerError::Deadline)
                ));
                assert_eq!(
                    control.check().unwrap_err(),
                    Error::Authority(AuthorityError::ObservationExpired)
                );
                let finished = host_now(&clock).unwrap();
                assert!(finished >= original_until);
                // Falling back to the unrelated two-second work ceiling is not
                // timely authority expiry, even though it also kills the child.
                assert!(finished.as_micros() < before.as_micros() + 2_000_000);
                assert!(control.renew(501).is_err());
            }
            assert_eq!(source.worker_mut().state(), State::Poisoned);
            reap_failed(&rt, &mut source).await;
        }
    });
}

// Startup, as running capture: each start exchange keeps one fixed two-second
// work ceiling and re-reads the original authority on every poll. A timely
// renewal carries a start slower than the first sampled authority cap; with no
// renewal that cap still ends it.

#[test]
fn renewed_observation_keeps_a_slow_capture_start_past_the_old_cap() {
    run_shared!(rt, clock, {
        for (id, renew) in [(306, true), (307, false)] {
            let control = observation(&rt, id);
            let original_until = near_expiry(&clock, &control).await;
            if renew {
                let (result, responses) =
                    with_renewals(&clock, &control, start_source(&control, "slow-start")).await;
                let mut source = result.unwrap();
                assert!(responses >= 2);
                assert!(host_now(&clock).unwrap() >= original_until);
                assert!(control.observation_until().unwrap() > original_until);
                control.check().unwrap();
                stop(&mut source, &clock).await;
            } else {
                let error = refused(start_source(&control, "slow-start").await);
                assert!(expired(error), "{error:?}");
                assert!(host_now(&clock).unwrap() >= original_until);
            }
        }
    });
}

#[test]
fn renewals_cannot_carry_a_silent_capture_start_past_its_two_second_ceiling() {
    run_shared!(rt, clock, {
        let control = observation(&rt, 308);
        let before = host_now(&clock).unwrap();
        let (result, responses) =
            with_renewals(&clock, &control, start_source(&control, "silent-start")).await;
        assert_eq!(refused(result), Error::Worker(WorkerError::Deadline));
        assert!(responses >= 5);
        // Not the first sampled one-second authority cap: the work ceiling.
        assert!(host_now(&clock).unwrap().as_micros() >= before.as_micros() + 2_000_000);
        control.check().unwrap();
    });
}

#[test]
fn renewed_observation_keeps_a_slow_monitor_discovery_past_the_old_cap() {
    run_shared!(rt, clock, {
        for (id, renew) in [(309, true), (310, false)] {
            let control = observation(&rt, id);
            let original_until = near_expiry(&clock, &control).await;
            if renew {
                let (result, responses) =
                    with_renewals(&clock, &control, discover(&control, "slow-discover")).await;
                let discovered = result.unwrap();
                assert!(responses >= 2);
                assert!(host_now(&clock).unwrap() >= original_until);
                control.check().unwrap();
                // The same child still configures and stops normally.
                let catalog = discovered.catalog().unwrap();
                assert_eq!(catalog.displays().len(), 2);
                let choice = catalog.selection(catalog.displays()[0].handle).unwrap();
                let mut source = discovered
                    .configure_local(choice, configuration())
                    .unwrap()
                    .await
                    .unwrap();
                stop(&mut source, &clock).await;
            } else {
                let error = refused(discover(&control, "slow-discover").await);
                assert!(expired(error), "{error:?}");
                assert!(host_now(&clock).unwrap() >= original_until);
            }
        }
    });
}

#[test]
fn renewed_observation_keeps_a_slow_monitor_configuration_past_the_old_cap() {
    run_shared!(rt, clock, {
        for (id, renew) in [(311, true), (312, false)] {
            let control = observation(&rt, id);
            let discovered = discover(&control, "slow-configure").await.unwrap();
            let catalog = discovered.catalog().unwrap();
            let choice = catalog.selection(catalog.displays()[0].handle).unwrap();
            let original_until = near_expiry(&clock, &control).await;
            let configuring = discovered.configure_local(choice, configuration()).unwrap();
            if renew {
                let (result, responses) = with_renewals(&clock, &control, configuring).await;
                let mut source = result.unwrap();
                assert!(responses >= 2);
                assert!(host_now(&clock).unwrap() >= original_until);
                control.check().unwrap();
                stop(&mut source, &clock).await;
            } else {
                let error = refused(configuring.await);
                assert!(expired(error), "{error:?}");
                assert!(host_now(&clock).unwrap() >= original_until);
            }
        }
    });
}

#[test]
fn a_queued_launch_services_renewals_before_the_pool_can_claim_it() {
    run_shared!(rt, clock, {
        let control = observation(&rt, 313);
        let mut pool = worker_pool::HeldPool::new(&clock).await;
        let original_until = near_expiry(&clock, &control).await;
        let (result, responses) = with_renewals(
            &clock,
            &control,
            during_pending(
                &clock,
                Duration::from_millis(700),
                start_source(&control, "direct"),
                || pool.release(),
            ),
        )
        .await;
        let mut source = result.unwrap();
        assert!(responses >= 2);
        assert!(host_now(&clock).unwrap() >= original_until);
        assert!(control.observation_until().unwrap() > original_until);
        control.check().unwrap();
        pool.finish().await;
        stop(&mut source, &clock).await;
    });
}
