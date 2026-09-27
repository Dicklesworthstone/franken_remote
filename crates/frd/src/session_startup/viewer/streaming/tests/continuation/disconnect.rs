//! Original TLS/UDP and supervised decoder IPC; media/cleanup replies are fixtures.
use super::*;
use crate::session_startup::{HostSession, StreamingViewerError};
use fr_media::delivery::{BudgetUsage, MediaBudget, ReceiveState};
use fr_transport::quic::{self, ClosedReport};
use fr_wire::{
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects, Reason},
};
use std::ops::ControlFlow;

async fn streaming(c: &Cx, h: &Cx) -> (HostSession, StreamingViewer, SendCache) {
    let (mut host, mut viewer) = observing_pair(c, h, capabilities()).await;
    let (_, vm) = media_pair(&mut host, &mut viewer, c, h).await;
    let cfg = vm
        .receiver_config(&viewer.transport, ReceivePolicy::default())
        .unwrap();
    let mut receiver =
        ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
    let mut presenter = Presenter::stream_fixture(c, &viewer.transport, &vm, &mut receiver).await;
    let mut cache =
        SendCache::new(cfg.limits, cfg.bindings, cfg.epoch, SendPolicy::default()).unwrap();
    seed(&mut cache, &mut receiver, 0);
    let initial = presenter
        .present_next(c, &mut receiver)
        .await
        .unwrap()
        .unwrap();
    // Complete ordinary attachment ACK traffic before testing terminal custody.
    for _ in 0..4 {
        let (a, b) = support::both(
            host.drive(
                Duration::from_millis(1),
                || Ok(u128::from(stamp()) + 1),
                |_, _| Ok(Disposition::Blocked),
            ),
            viewer.drive(Duration::from_millis(1), |_, _| Ok(Disposition::Blocked)),
        )
        .await;
        a.unwrap();
        b.unwrap();
    }
    let viewer = StreamingViewer::from_test_parts(viewer, vm, presenter, receiver, initial);
    (host, viewer, cache)
}
fn seed(cache: &mut SendCache, receiver: &mut ReceivePipeline, frame: u64) {
    put(
        cache,
        frame,
        frame.checked_sub(1),
        if frame == 0 {
            DeliveryMode::Recovery
        } else {
            DeliveryMode::Datagrams
        },
        stamp(),
    );
    let mut bytes = [0; 1150];
    while let Some(packet) = cache.next_packet(stamp(), &mut bytes).unwrap() {
        receiver
            .receive(packet.channel(), &bytes[..packet.byte_len()], stamp())
            .unwrap();
    }
}
fn report() -> Closed {
    // Explicit host accounting fixture, deliberately NOT a local child-exit claim.
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Known {
            pending: 2,
            uncertain: 7,
        },
    }
}
fn arm(host: &mut HostSession, cleanup: &Cx) -> ClosedReport {
    let control = host.observation().unwrap();
    let binding = host.binding();
    let (q, routes) = host.io().unwrap();
    let original = q.binding();
    let owner = ClosedReport::default();
    q.arm_closed_report(
        cleanup,
        &original,
        routes.outbound,
        Binding {
            channel: binding.id,
            session: binding.remote_session,
        },
        owner.registration(),
        move || control.revoke(),
    )
    .unwrap();
    owner
}
async fn host_close(
    host: &mut HostSession,
    owner: ClosedReport,
) -> Option<Result<(), quic::Error>> {
    loop {
        if host
            .drive(
                Duration::from_millis(1),
                || Ok(u128::from(stamp()) + 100),
                |_, _| panic!("close escaped canonical dispatcher"),
            )
            .await
            .is_err()
        {
            break;
        }
    }
    assert!(host.observation().is_err());
    owner.finish(report()).await
}
async fn reaped(viewer: &mut StreamingViewer, cleanup: &Cx) {
    assert_eq!(viewer.receiver.state(), ReceiveState::Closed);
    assert!(viewer.control().is_stopped());
    viewer
        .reap_media(
            cleanup,
            crate::worker::Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.budget_usage(), BudgetUsage::default());
}
#[test]
fn disconnect_fences_media_at_call_time_and_retains_original_decoder_for_reap() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer, mut cache) = Box::pin(streaming(&c, &h)).await;
        seed(&mut cache, &mut viewer.receiver, 1);
        let held = viewer.receiver.take_decodable(stamp()).unwrap().unwrap();
        let charged = viewer.budget_usage();
        let pid = viewer.worker_id();
        let owner = arm(&mut host, &cleanup);
        let stop = viewer.control();
        let close = viewer.disconnect(Reason::Requested);
        assert!(!held.is_live()); // Before polling; the borrowed bytes stay owned.
        assert!(!stop.is_stopped()); // Only the closing exchange still uses this context.
        let (delivery, outcome) =
            Box::pin(support::both(host_close(&mut host, owner), close)).await;
        let outcome = outcome.unwrap();
        assert_eq!(delivery, Some(Ok(())));
        assert_eq!(outcome.report, Some(report()));
        assert_eq!(outcome.transport, Ok(()));
        assert_eq!(viewer.worker_id(), pid);
        assert_eq!(viewer.budget_usage(), charged);
        assert_eq!(viewer.disconnect_outcome(), Some(outcome));
        assert!(viewer.disconnect(Reason::Requested).await.is_err());
        assert_eq!(viewer.disconnect_outcome(), Some(outcome));
        drop(held);
        reaped(&mut viewer, &cleanup).await;
        assert_eq!(viewer.disconnect_outcome(), Some(outcome));
    });
}
#[test]
fn observation_ui_can_close_after_real_decode_without_another_application_callback() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer, mut cache) = Box::pin(streaming(&c, &h)).await;
        seed(&mut cache, &mut viewer.receiver, 1);
        let owner = arm(&mut host, &cleanup);
        let mut events = Vec::new();
        let close = viewer.serve_until(|event| {
            if let Some(event) = event {
                events.push(event.frame.as_raw());
                Ok(ControlFlow::Break(Reason::Requested))
            } else {
                Ok(ControlFlow::Continue(()))
            }
        });
        let (delivery, outcome) =
            Box::pin(support::both(host_close(&mut host, owner), close)).await;
        assert_eq!(delivery, Some(Ok(())));
        assert_eq!(outcome.unwrap().report, Some(report()));
        assert_eq!(events, [1]);
        assert_eq!(viewer.statistics().decoded, 1);
        reaped(&mut viewer, &cleanup).await;
    });
}
#[test]
fn local_close_during_pending_decode_aborts_without_publishing_a_late_receipt() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer, mut cache) = Box::pin(streaming(&c, &h)).await;
        seed(&mut cache, &mut viewer.receiver, 1);
        let owner = arm(&mut host, &cleanup);
        let mut calls = 0;
        let close = viewer.serve_until(|event| {
            assert!(event.is_none()); // Fixture decoder waits 90ms; closure does not.
            calls += 1;
            Ok(if calls == 2 {
                ControlFlow::Break(Reason::Requested)
            } else {
                ControlFlow::Continue(())
            })
        });
        let (delivery, outcome) =
            Box::pin(support::both(host_close(&mut host, owner), close)).await;
        assert_eq!(delivery, Some(Ok(())));
        assert_eq!(outcome.unwrap().report, Some(report()));
        assert_eq!(calls, 2);
        assert_eq!(viewer.statistics().decoded, 0);
        reaped(&mut viewer, &cleanup).await;
    });
}
#[test]
fn unpolled_close_and_unpolled_serve_until_are_terminal_without_completed_outcomes() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (host, mut viewer, _) = Box::pin(streaming(&c, &h)).await;
        drop(viewer.disconnect(Reason::Requested));
        assert_eq!(viewer.disconnect_outcome(), None);
        reaped(&mut viewer, &cleanup).await;
        drop(host);
    });
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (host, mut viewer, _) = Box::pin(streaming(&c, &h)).await;
        drop(viewer.serve_until(|_| panic!("unpolled callback")));
        assert_eq!(viewer.disconnect_outcome(), None);
        reaped(&mut viewer, &cleanup).await;
        drop(host);
    });
}
#[test]
fn emergency_stop_and_callback_failure_are_not_relabelled_orderly_disconnect() {
    for emergency in [false, true] {
        run(|c, h| async move {
            let cleanup = Cx::current().unwrap();
            let (host, mut viewer, _) = Box::pin(streaming(&c, &h)).await;
            let stop = viewer.control();
            let result = viewer
                .serve_until(|_| {
                    if emergency {
                        stop.stop();
                        Ok(ControlFlow::Continue(()))
                    } else {
                        Err(())
                    }
                })
                .await;
            assert!(result.is_err());
            if !emergency {
                assert_eq!(result, Err(StreamingViewerError::Application));
            }
            assert_eq!(viewer.disconnect_outcome(), None);
            reaped(&mut viewer, &cleanup).await;
            drop(host);
        });
    }
}
#[test]
fn control_intent_cannot_select_observation_shutdown_or_execute_its_ui() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (host, mut viewer, _) = Box::pin(streaming(&c, &h)).await;
        // Explicit local role-state fixture: no input grant is fabricated.
        let super::super::super::Peer::Observe { session, .. } = &mut viewer.peer else {
            panic!()
        };
        session.opened.selection.role = Role::RequestControl;
        assert_eq!(
            viewer.serve_until(|_| panic!("forbidden UI")).await,
            Err(StreamingViewerError::Session(
                crate::session_startup::Error::Order
            ))
        );
        assert_eq!(viewer.disconnect_outcome(), None);
        reaped(&mut viewer, &cleanup).await;
        drop(host);
    });
}
