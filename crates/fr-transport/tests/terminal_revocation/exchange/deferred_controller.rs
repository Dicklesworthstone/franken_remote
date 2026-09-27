//! Deferred local intent with real UDP/TLS. The synchronous input fence remains
//! an explicit fixture; transport does not certify native key cleanup.
use super::*;
use fr_transport::{ControlCloseReport, ControlCloseSignal};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

fn arm(
    p: &mut Pair,
    app: &Cx,
    cleanup: &Cx,
    until: u64,
) -> (ControlCloseReport, ControlCloseSignal, Arc<AtomicBool>) {
    let reporter = ControlCloseReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let flag = fenced.clone();
    let app = app.clone();
    let original = p.client.binding();
    let route = routes(p);
    let signal = p
        .client
        .arm_control_close_request(
            cleanup,
            &original,
            route,
            binding(),
            report().lease,
            until,
            reporter.registration(),
            move || {
                flag.store(true, Ordering::Release);
                app.cancel_fast(CancelKind::User);
            },
        )
        .unwrap();
    (reporter, signal, fenced)
}

#[test]
fn native_request_fences_immediately_and_normal_close_transfers_the_exact_exchange() {
    let rt = runtime();
    let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&cleanup).await;
        let (reporter, signal, fenced) = arm(&mut p, &app, &cleanup, clock(&cleanup) + 1_000_000);
        assert!(!signal.is_requested());
        assert!(!fenced.load(Ordering::Acquire));
        assert_eq!(signal.request(closure::Reason::Requested), Ok(true));
        assert!(fenced.load(Ordering::Acquire));
        assert!(app.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        assert_eq!(signal.request(closure::Reason::ClientFailure), Ok(false));
        assert_eq!(p.client.tick(&app, || true), Err(Error::Cancelled));
        assert!(p.client.is_closed());
        let (outcome, sent) = Box::pin(both(
            reporter.finish(),
            request_then_report(&cleanup, &mut p, final_report()),
        ))
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(outcome.exchange.report, Some(final_report()));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.transport, Ok(()));
        assert_eq!(sent, Ok(()));
        assert!(app.is_cancel_requested());
    });
}

#[test]
fn pending_native_io_returns_only_terminal_custody_after_the_local_input_fence() {
    let rt = runtime();
    let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&cleanup).await;
        let (reporter, signal, fenced) = arm(&mut p, &app, &cleanup, clock(&cleanup) + 1_000_000);
        let mut exercised = false;
        for _ in 0..16 {
            let mut io = pin!(p.client.drive(&app, Duration::from_millis(100), || true));
            if poll_fn(|task| Poll::Ready(io.as_mut().poll(task).is_pending())).await {
                assert_eq!(signal.request(closure::Reason::Requested), Ok(true));
                assert!(fenced.load(Ordering::Acquire));
                assert_eq!(io.await, Err(Error::Cancelled));
                exercised = true;
                break;
            }
        }
        assert!(exercised);
        let (outcome, sent) = Box::pin(both(
            reporter.finish(),
            request_then_report(&cleanup, &mut p, final_report()),
        ))
        .await;
        assert_eq!(outcome.unwrap().exchange.report, Some(final_report()));
        assert_eq!(sent, Ok(()));
        assert!(p.client.is_closed());
    });
}

#[test]
fn deadline_refresh_is_allowed_before_intent_but_never_after_it() {
    let rt = runtime();
    let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&cleanup).await;
        let at = clock(&cleanup);
        let (reporter, signal, _) = arm(&mut p, &app, &cleanup, at + 1_000);
        signal.update_deadline(at + 20_000).unwrap();
        asupersync::time::sleep(cleanup.now(), Duration::from_millis(3)).await;
        assert_eq!(signal.request(closure::Reason::Requested), Ok(true));
        signal.update_deadline(u64::MAX).unwrap();
        p.client.close();
        let finish = reporter.finish();
        asupersync::time::sleep(cleanup.now(), Duration::from_millis(25)).await;
        assert_eq!(
            finish.await.unwrap().exchange.transport,
            Err(Error::Expired)
        );
        assert!(p.client.is_closed());
    });
}

#[test]
fn emergency_close_and_abandoned_consumers_never_invent_a_local_request() {
    let rt = runtime();
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let mut p = pair(&cleanup).await;
        let (reporter, signal, fenced) = arm(&mut p, &app, &cleanup, u64::MAX);
        p.client.close();
        assert!(fenced.load(Ordering::Acquire));
        assert!(!signal.is_requested());
        assert_eq!(reporter.finish().await, None);
        let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let mut p = pair(&cleanup).await;
        let (reporter, signal, fenced) = arm(&mut p, &app, &cleanup, u64::MAX);
        drop(reporter);
        assert_eq!(
            signal.request(closure::Reason::Requested),
            Err(Error::Closed)
        );
        assert!(fenced.load(Ordering::Acquire));
        p.client.close();
        assert!(p.client.is_closed());
    });
}

#[test]
fn abandoned_native_io_is_not_recovered_by_a_late_request_or_finish() {
    let rt = runtime();
    let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&cleanup).await;
        let (reporter, signal, _) = arm(&mut p, &app, &cleanup, u64::MAX);
        let mut exercised = false;
        for _ in 0..16 {
            let mut io = pin!(p.client.drive(&app, Duration::from_millis(100), || true));
            if poll_fn(|task| Poll::Ready(io.as_mut().poll(task).is_pending())).await {
                signal.request(closure::Reason::Requested).unwrap();
                exercised = true;
                break; // Drop the live operation, rather than collect cancellation.
            }
        }
        assert!(exercised);
        p.client.close();
        assert_eq!(
            reporter.finish().await.unwrap().exchange.transport,
            Err(Error::Closed)
        );
        assert!(p.client.is_closed());
    });
}

#[test]
fn native_backlog_and_security_loss_still_refuse_after_the_fence() {
    let rt = runtime();
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        for backlog in [true, false] {
            let app = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
            let mut p = pair(&cleanup).await;
            let allowed = Arc::new(AtomicBool::new(true));
            let gate = allowed.clone();
            p.client
                .retain_lifetime_check(&cleanup, Arc::new(move || gate.load(Ordering::Acquire)))
                .unwrap();
            let (reporter, signal, fenced) = arm(&mut p, &app, &cleanup, u64::MAX);
            if backlog {
                let route = routes(&p).outbound;
                p.client
                    .send(
                        &cleanup,
                        Route::Stream(route),
                        &request_bytes(closure::Reason::ClientFailure),
                        clock(&cleanup) + 1_000_000,
                        || true,
                    )
                    .unwrap();
                p.client
                    .drive(&cleanup, Duration::ZERO, || true)
                    .await
                    .unwrap();
                assert!(p.client.usage().retained_send_records > 0);
            } else {
                allowed.store(false, Ordering::Release);
            }
            signal.request(closure::Reason::Requested).unwrap();
            p.client.close();
            assert!(fenced.load(Ordering::Acquire));
            let outcome = reporter.finish().await.unwrap();
            assert_eq!(
                outcome.exchange.transport,
                Err(if backlog {
                    Error::Backpressure
                } else {
                    Error::Unauthorized
                })
            );
            assert_eq!(outcome.exchange.report, None);
            assert_eq!(outcome.revocation, None);
        }
    });
}

#[test]
fn wrong_connection_and_competing_registrations_cannot_replace_the_original_slot() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let first = ControlCloseReport::default();
        let route = routes(&p);
        let other = p.server.binding();
        assert!(matches!(
            p.client.arm_control_close_request(
                &cx,
                &other,
                route,
                binding(),
                report().lease,
                u64::MAX,
                first.registration(),
                || panic!("foreign fence")
            ),
            Err(Error::WrongRoute)
        ));
        assert!(!p.client.is_closed());
        let original = p.client.binding();
        let _signal = p
            .client
            .arm_control_close_request(
                &cx,
                &original,
                route,
                binding(),
                report().lease,
                u64::MAX,
                first.registration(),
                || {},
            )
            .unwrap();
        let second = ControlCloseReport::default();
        assert!(matches!(
            p.client.arm_control_close_request(
                &cx,
                &original,
                route,
                binding(),
                report().lease,
                u64::MAX,
                second.registration(),
                || panic!("competing fence")
            ),
            Err(Error::InvalidPolicy)
        ));
        assert_eq!(
            p.client.arm_closed_report(
                &cx,
                &original,
                route.outbound,
                binding(),
                ClosedReport::default().registration(),
                || panic!("other terminal kind")
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!p.client.is_closed());
        p.client.close();
        assert_eq!(first.finish().await, None);
        assert_eq!(second.finish().await, None);
    });
}
