//! Original UDP/TLS/session and supervised decoder ownership. Media pixels,
//! native input and host cleanup accounting remain explicit fixture evidence.
use super::*;
use crate::session_startup::{InteractiveCloseOutcome, InteractiveViewerState};
use fr_transport::quic::ClosedReport;
use fr_wire::{
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects, Reason},
};
use std::ops::ControlFlow;

async fn settled(c: &Cx, h: &Cx) -> Fixture {
    let mut f = Box::pin(fixture(c, h)).await;
    // A fixed number of drive turns is not proof that native retransmission
    // custody ended. The terminal report must not flush old application data.
    // Drain ordinary setup work BEFORE closing, under one fixed fixture bound.
    let until = now(c).unwrap() + 500_000;
    let mut nonce = 91_000_u128;
    for _ in 0..500 {
        assert!(
            now(c).unwrap() < until,
            "setup records were not acknowledged"
        );
        let (host, viewer) = Box::pin(support::both(
            f.host.drive(
                Duration::from_millis(1),
                || {
                    nonce += 1;
                    Ok(nonce)
                },
                block,
            ),
            f.stream
                .peer
                .parts()
                .unwrap()
                .0
                .drive(Duration::from_millis(1), block),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
        let host = f.host.io().unwrap().0.usage();
        let viewer = f.stream.peer.parts().unwrap().0.io().unwrap().0.usage();
        if [host, viewer]
            .into_iter()
            .all(|usage| usage.retained_send_records == 0 && usage.retained_send_upper_bound == 0)
        {
            return f;
        }
    }
    panic!("setup records were not acknowledged within the bounded turn count")
}

fn arm(host: &mut HostSession, cleanup: &Cx) -> ClosedReport {
    let control = host.observation().unwrap();
    let binding = host.binding();
    let (q, routes) = host.io().unwrap();
    let original = q.binding();
    let report = ClosedReport::default();
    q.arm_closed_report(
        cleanup,
        &original,
        routes.outbound,
        Binding {
            channel: binding.id,
            session: binding.remote_session,
        },
        report.registration(),
        move || control.revoke(),
    )
    .unwrap();
    report
}

async fn finish_host(
    host: &mut HostSession,
    owner: ClosedReport,
    report: Closed,
) -> Option<Result<(), quic::Error>> {
    while host
        .drive(
            Duration::from_millis(1),
            || Ok(92_001),
            |_, _| panic!("no control request or input may precede the close"),
        )
        .await
        .is_ok()
    {}
    assert!(host.observation().is_err());
    owner.finish(report).await
}

async fn reap(stream: &mut StreamingViewer, cleanup: &Cx) {
    assert!(stream.control().is_stopped());
    assert_eq!(stream.receiver.state(), ReceiveState::Closed);
    stream
        .reap_media(
            cleanup,
            Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream.budget_usage(), BudgetUsage::default());
}

#[test]
fn prepared_clock_enabled_close_keeps_only_terminal_io_live_until_original_guard_drop() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(settled(&c, &h)).await;
        // Exercise the same original teardown guard and preparation primitive
        // used by disconnect_unrequested, with its actual negotiated scope.
        let ending = Operation {
            viewer: &mut f.stream,
        };
        let (session, _) = ending.viewer.peer.parts().unwrap();
        super::super::validate(session, &f.channels, f.request).unwrap();
        assert!(session.clock_correlation().unwrap().is_some());
        let prepared = session.begin_disconnect_before_control(Reason::Requested);
        assert!(session.is_closed(), "ordinary session admission is fenced");
        assert!(session.transport.is_closed());
        assert_eq!(
            session.transport.tick(&c, || true),
            Err(quic::Error::Closed)
        );
        assert!(
            c.checkpoint().is_ok(),
            "stopping the clock must not cancel the original closing context"
        );
        drop(prepared);
        drop(ending);
        assert!(c.checkpoint().is_err());
        assert!(f.stream.disconnect_outcome().is_none());
        reap(&mut f.stream, &cleanup).await;
    });
}

#[test]
fn ordinary_session_reentry_refuses_and_cancels_the_prepared_close() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(settled(&c, &h)).await;
        let ending = Operation {
            viewer: &mut f.stream,
        };
        let (session, _) = ending.viewer.peer.parts().unwrap();
        super::super::validate(session, &f.channels, f.request).unwrap();
        let prepared = session.begin_disconnect_before_control(Reason::Requested);
        assert!(c.checkpoint().is_ok());
        assert_eq!(
            session.io().err(),
            Some(crate::session_startup::Error::Closed)
        );
        assert!(c.checkpoint().is_err());
        assert_eq!(
            prepared.await.unwrap().transport,
            Err(quic::Error::Cancelled)
        );
        drop(ending);
        reap(&mut f.stream, &cleanup).await;
    });
}

#[test]
fn the_original_external_stop_still_cancels_a_prepared_clock_enabled_close() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(settled(&c, &h)).await;
        let ending = Operation {
            viewer: &mut f.stream,
        };
        let (session, _) = ending.viewer.peer.parts().unwrap();
        super::super::validate(session, &f.channels, f.request).unwrap();
        let stop = session.control();
        let prepared = session.begin_disconnect_before_control(Reason::Requested);
        assert!(c.checkpoint().is_ok());
        stop.stop();
        assert!(c.checkpoint().is_err());
        assert_eq!(
            prepared.await.unwrap().transport,
            Err(quic::Error::Cancelled)
        );
        drop(ending);
        reap(&mut f.stream, &cleanup).await;
    });
}

#[test]
fn unrequested_interactive_viewing_closes_on_the_original_session_without_a_lease() {
    for reason in [ClosedReason::ClientRequested, ClosedReason::PermissionLost] {
        run(|c, h| async move {
            let cleanup = Cx::current().unwrap();
            let mut f = Box::pin(settled(&c, &h)).await;
            let worker = f.stream.worker_id();
            let report = Closed {
                reason,
                cleanup: Cleanup::Unconfirmed,
                effects: OutstandingEffects::Known {
                    pending: 2,
                    uncertain: 7,
                },
            };
            let owner = arm(&mut f.host, &cleanup);
            let mut callbacks = 0;
            let close = f.stream.serve_interactive_session_until(
                f.channels,
                f.request,
                Policy::default(),
                &cleanup,
                |state, _| {
                    assert!(matches!(state, InteractiveViewerState::Viewing(_)));
                    callbacks += 1;
                    assert_eq!(callbacks, 1, "no callback after the local close choice");
                    Ok(ControlFlow::Break(Reason::Requested))
                },
                |_| panic!("no input owner or effect exists"),
                block,
            );
            let (outcome, sent) = Box::pin(support::both(
                close,
                finish_host(&mut f.host, owner, report),
            ))
            .await;
            let InteractiveCloseOutcome::Viewing(outcome) = outcome.unwrap() else {
                panic!("advertisement cannot fabricate a granted controller");
            };
            assert!(
                c.checkpoint().is_err(),
                "completed teardown cancels its original context"
            );
            assert_eq!(sent, Some(Ok(())));
            assert_eq!(outcome.transport, Ok(()));
            assert_eq!(
                outcome.report,
                Some(report),
                "never upgrade host cleanup/effects"
            );
            assert_eq!(f.stream.disconnect_outcome(), Some(outcome));
            assert!(f.stream.control_disconnect_outcome().is_none());
            assert_eq!(f.stream.pending_control_actions(), 0);
            assert_eq!(f.stream.worker_id(), worker);
            assert!(!f.seat.is_occupied());
            assert!(f.broker.request().is_none());
            reap(&mut f.stream, &cleanup).await;
            assert_eq!(f.stream.disconnect_outcome(), Some(outcome));
        });
    }
}

#[test]
fn a_request_staged_in_the_same_callback_cannot_be_relabelled_as_viewing_close() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(settled(&c, &h)).await;
        let result = f
            .stream
            .serve_interactive_session_until(
                f.channels,
                f.request,
                Policy::default(),
                &cleanup,
                |state, _| {
                    let InteractiveViewerState::Viewing(viewing) = state else {
                        panic!("request must still be local to this callback");
                    };
                    viewing.request_control().unwrap();
                    Ok(ControlFlow::Break(Reason::Requested))
                },
                |_| panic!("no grant"),
                block,
            )
            .await;
        assert_eq!(result, Err(Error::Closed));
        assert!(f.stream.disconnect_outcome().is_none());
        assert!(f.stream.control_disconnect_outcome().is_none());
        assert!(f.broker.request().is_none());
        assert!(!f.seat.is_occupied());
        reap(&mut f.stream, &cleanup).await;
    });
}

#[test]
fn a_pending_control_request_remains_terminal_when_the_ui_closes() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut stream,
            host,
            broker,
            channels,
            media,
            observation,
            seat,
            request,
            descriptor,
            video,
            ..
        } = Box::pin(settled(&c, &h)).await;
        let done = Cell::new(false);
        let effects = Arc::new(Mutex::new(Vec::new()));
        let mut requested = false;
        let mut pending = false;
        let (result, ()) = Box::pin(support::both(
            async {
                let result = stream
                    .serve_interactive_session_until(
                        channels,
                        request,
                        Policy::default(),
                        &cleanup,
                        |state, _| match state {
                            InteractiveViewerState::Viewing(viewing) => {
                                assert!(!requested);
                                viewing.request_control().unwrap();
                                requested = true;
                                Ok(ControlFlow::Continue(()))
                            }
                            InteractiveViewerState::Requesting(_) => {
                                pending = true;
                                Ok(ControlFlow::Break(Reason::Requested))
                            }
                            InteractiveViewerState::Controlled(_) => panic!("approval is delayed"),
                        },
                        |_| panic!("no grant"),
                        block,
                    )
                    .await;
                done.set(true);
                result
            },
            host_loop(
                host,
                broker,
                &media,
                &observation,
                video,
                descriptor,
                &done,
                effects.clone(),
                2_000_000,
                false,
            ),
        ))
        .await;
        assert!(requested && pending);
        assert_eq!(result, Err(Error::Closed));
        assert!(stream.disconnect_outcome().is_none());
        assert!(stream.control_disconnect_outcome().is_none());
        assert!(effects.lock().unwrap().is_empty());
        assert!(!seat.is_occupied());
        reap(&mut stream, &cleanup).await;
    });
}

#[test]
fn unpolled_or_failed_ui_close_never_creates_success_and_retains_native_cleanup() {
    for polled in [false, true] {
        run(|c, h| async move {
            let cleanup = Cx::current().unwrap();
            let mut f = Box::pin(settled(&c, &h)).await;
            let worker = f.stream.worker_id();
            let future = f.stream.serve_interactive_session_until(
                f.channels,
                f.request,
                Policy::default(),
                &cleanup,
                |_, _| Err(()),
                |_| panic!("no grant"),
                block,
            );
            if polled {
                assert_eq!(future.await, Err(Error::Application));
            } else {
                drop(future);
            }
            assert!(f.stream.disconnect_outcome().is_none());
            assert!(f.stream.control_disconnect_outcome().is_none());
            assert_eq!(f.stream.worker_id(), worker);
            assert!(!f.seat.is_occupied());
            reap(&mut f.stream, &cleanup).await;
        });
    }
}

#[test]
fn legacy_control_close_keeps_its_terminal_pre_grant_contract() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(settled(&c, &h)).await;
        let result = f
            .stream
            .serve_interactive_control_until(
                f.channels,
                f.request,
                Policy::default(),
                &cleanup,
                |state, _| {
                    assert!(matches!(state, InteractiveViewerState::Viewing(_)));
                    Ok(ControlFlow::Break(Reason::Requested))
                },
                |_| panic!("no grant"),
                block,
            )
            .await;
        assert_eq!(result, Err(Error::Closed));
        assert!(f.stream.disconnect_outcome().is_none());
        assert!(f.stream.control_disconnect_outcome().is_none());
        reap(&mut f.stream, &cleanup).await;
    });
}
