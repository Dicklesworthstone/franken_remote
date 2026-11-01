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
    // Retire ordinary attachment/clock ACK work before terminal custody.
    for _ in 0..4 {
        let (host, viewer) = Box::pin(support::both(
            f.host.drive(Duration::from_millis(1), || Ok(91_001), block),
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
    }
    f
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
