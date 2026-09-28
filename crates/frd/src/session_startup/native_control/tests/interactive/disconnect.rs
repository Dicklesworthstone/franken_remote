//! Full public native bootstrap, managed host input ownership and closing. Native
//! capture/decode, visibility and OS effects are explicit fixtures, not hardware.
use super::*;
use crate::session_startup::{
    InteractiveViewerState, ManagedHostControlState, NativeObserver, NativePublisher,
    ObserverError, StreamingViewerError,
};
use fr_transport::quic;
use fr_wire::{
    closure,
    lease_revoked::{CleanupStage, EffectStage},
};
use std::ops::ControlFlow;

async fn bootstrap(c: &Cx, h: &Cx) -> (NativePublisher, NativeObserver) {
    let (host, viewer) = pair_initialized(c, h, capabilities(), |_| {}).await;
    let mut nonce = 130_000;
    let (host, viewer) = Box::pin(support::both(
        host.publish_controlled_display(
            launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            config,
            || {
                nonce += 1;
                Ok(nonce)
            },
        ),
        viewer.observe_for_control(
            launch(WorkerRole::Present),
            ObserverPolicy::default(),
            ClockPolicy::default(),
            |catalog| Ok(Some(catalog.displays()[0].handle)),
        ),
    ))
    .await;
    (host.unwrap(), viewer.unwrap())
}
async fn reap(host: &mut NativePublisher, viewer: &mut NativeObserver, cleanup: &Cx) {
    let deadline = Deadline::after(cleanup, Duration::from_secs(1)).unwrap();
    host.reap_media(cleanup, deadline).await.unwrap();
    viewer.reap_input_capture(cleanup, deadline).await.unwrap();
    viewer.reap_media(cleanup, deadline).await.unwrap();
    assert_eq!(
        viewer.budget_usage(),
        fr_media::delivery::BudgetUsage::default()
    );
}
fn key(pressed: bool) -> fr_client::input::Action<'static> {
    fr_client::input::Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Orderly,
    CallbackFailure,
    Emergency,
    CancelledCleanup,
}

#[allow(clippy::too_many_lines)]
async fn granted(c: Cx, h: Cx, cleanup: Cx, end: End) {
    let (mut host, mut viewer) = Box::pin(bootstrap(&c, &h)).await;
    let caps = Capabilities::default().with(Capability::Keys);
    let target = host.control_target(caps).unwrap();
    let request = viewer.control_request(401, caps).unwrap();
    let pids = (host.worker_id(), viewer.worker_id());
    let source = host.control();
    let stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let receipts = Cell::new(0);
    let received_at = Cell::new(None);
    let closing = Cell::new(false);
    let requested = Cell::new(false);
    let mut viewing_turns = 0;
    let mut approved = false;
    let mut pressed = false;
    let mut shown = None;
    let mut nonce = 140_000;
    let mut ticket = 150_000;
    let started = now(&c).unwrap();
    let report_cx = if end == End::CancelledCleanup {
        &c
    } else {
        &cleanup
    };
    let (host_report, viewer_result) = Box::pin(support::both(
        async {
            let report = host
                .serve_managed_control_with_cleanup(
                    &cleanup,
                    seat.clone(),
                    |state| {
                        if let ManagedHostControlState::Pending(mut pending) = state
                            && pending.request().is_some()
                            && pending.native_status().is_none()
                            && pending.view_ready()?
                        {
                            assert!(requested.get());
                            let effects = effects.clone();
                            pending.approve(
                                target,
                                || {
                                    Some((
                                        InputLeaseId::from_raw(1901),
                                        InputTicketId::from_raw(1902),
                                    ))
                                },
                                move || Ok(Sink(effects)),
                                |_| true,
                            )?;
                            approved = true;
                        }
                        Ok(Some(target))
                    },
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    || {
                        ticket += 1;
                        Some(InputTicketId::from_raw(ticket))
                    },
                )
                .await;
            // Do not cancel a peer still acknowledging a delivered report.
            if !matches!(report.revocation, Some(Ok(()) | Err(quic::Error::Expired))) {
                stop.stop();
            }
            report
        },
        async {
            let result = viewer
                .serve_interactive_control_until(
                    report_cx,
                    401,
                    caps,
                    fr_client::input::Policy::default(),
                    |state, frame| {
                        assert!(!closing.get(), "no callbacks after closing begins");
                        match state {
                            InteractiveViewerState::Viewing(viewing) => {
                                viewing_turns += 1;
                                assert!(!seat.is_occupied());
                                assert!(effects.lock().unwrap().is_empty());
                                assert_eq!(viewing.request(), request);
                                viewing
                                    .confirm_mapping(request.parent, target.view)
                                    .unwrap();
                                if let Some(p) = viewing.presentation()
                                    && shown != Some(p.frame)
                                {
                                    // Explicit platform visibility fixture, not decode=visible.
                                    crate::session_startup::confirm_visible(
                                        viewing.visible(p.frame.as_raw()),
                                    );
                                    shown = Some(p.frame);
                                }
                                if now(&c).unwrap() >= started + 120_000 && !requested.replace(true)
                                {
                                    viewing.request_control().unwrap();
                                }
                            }
                            InteractiveViewerState::Requesting(pending) => {
                                assert!(requested.get());
                                assert_eq!(pending.request(), request);
                            }
                            InteractiveViewerState::Controlled(input) => {
                                if let Some(frame) = frame {
                                    crate::session_startup::confirm_visible(
                                        input.visible(frame.frame.as_raw()),
                                    );
                                }
                                if !pressed {
                                    let _ = input.action(key(true)).unwrap();
                                    pressed = true;
                                }
                                // Allow the original request's native ACK epoch to
                                // settle, without extending deadlines or retrying actions.
                                if let Some(at) = received_at.get()
                                    && now(&c).unwrap() >= at + 85_000
                                {
                                    closing.set(true);
                                    match end {
                                        End::CallbackFailure => return Err(()),
                                        End::Emergency => {
                                            input.control().stop();
                                            return Ok(ControlFlow::Continue(()));
                                        }
                                        End::Orderly | End::CancelledCleanup => {
                                            // Encoded but never sent: host cleanup is
                                            // not a receipt for this pending release.
                                            let _ = input.action(key(false)).unwrap();
                                            return Ok(ControlFlow::Break(
                                                closure::Reason::Requested,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        Ok(ControlFlow::Continue(()))
                    },
                    |_| {
                        assert!(!closing.get());
                        receipts.set(receipts.get() + 1);
                        received_at.set(Some(now(&c).unwrap()));
                    },
                )
                .await;
            if end != End::Orderly {
                source.revoke();
            }
            result
        },
    ))
    .await;
    assert!(requested.get() && approved && pressed && closing.get());
    assert!(viewing_turns > 1);
    assert_eq!(
        receipts.get(),
        1,
        "cleanup must not invent an action receipt"
    );
    assert!(host_report.session.is_err());
    assert!(host_report.input.unwrap().handoff_safe(), "{host_report:?}");
    assert!(!seat.is_occupied());
    let ops = effects.lock().unwrap().clone();
    assert_eq!(ops.len(), 2, "original press then Driver cleanup release");
    assert!(matches!(
        ops[0],
        Operation::Key {
            transition: KeyTransition::Press,
            ..
        }
    ));
    assert!(matches!(
        ops[1],
        Operation::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    let retained = viewer.control_disconnect_outcome();
    match end {
        End::Orderly => {
            let outcome = viewer_result.unwrap();
            assert_eq!(outcome.exchange.transport, Ok(()));
            assert_eq!(outcome.exchange.report, None);
            let report = outcome.revocation.unwrap();
            assert_eq!(report.lease, InputLeaseId::from_raw(1901));
            assert_eq!(report.reason, fr_wire::lease_revoked::Reason::SessionEnded);
            assert_eq!(report.cleanup, CleanupStage::Fenced);
            assert_eq!(report.effects, EffectStage::Unknown);
            assert_eq!(retained, Some(outcome));
            assert_eq!(host_report.revocation, Some(Ok(())));
        }
        End::CancelledCleanup => {
            let outcome = viewer_result.unwrap();
            assert_eq!(outcome.exchange.transport, Err(quic::Error::Cancelled));
            assert_eq!(outcome.revocation, None);
            assert_eq!(outcome.exchange.report, None);
            assert_eq!(retained, Some(outcome));
        }
        End::CallbackFailure | End::Emergency => {
            assert!(viewer_result.is_err());
            if end == End::CallbackFailure {
                assert_eq!(
                    viewer_result,
                    Err(ObserverError::Streaming(StreamingViewerError::Application))
                );
            }
            assert_eq!(retained, None);
        }
    }
    let pending = usize::from(matches!(end, End::Orderly | End::CancelledCleanup));
    assert_eq!(viewer.pending_control_actions(), pending);
    let receipt = viewer.last_result();
    assert!(receipt.is_some());
    assert_eq!((host.worker_id(), viewer.worker_id()), pids);
    assert_eq!(viewer.statistics().decoded, 0, "no second bootstrap decode");
    assert!(viewer.presentation_reports() > 0);
    assert!(c.is_cancel_requested());
    assert!(!cleanup.is_cancel_requested());
    reap(&mut host, &mut viewer, &cleanup).await;
    assert_eq!(viewer.control_disconnect_outcome(), retained);
    assert_eq!(viewer.pending_control_actions(), pending);
    assert_eq!(viewer.last_result(), receipt);
}
#[test]
fn public_interactive_watch_request_grant_and_close_keep_original_lease_receipts_and_workers() {
    run3(|c, h, cleanup| granted(c, h, cleanup, End::Orderly));
}
#[test]
fn interactive_emergency_and_callback_errors_never_select_a_closing_exchange() {
    for end in [End::CallbackFailure, End::Emergency] {
        run3(|c, h, cleanup| granted(c, h, cleanup, end));
    }
}
#[test]
fn actual_interactive_grant_cannot_reuse_cancelled_application_context_for_closing() {
    run3(|c, h, cleanup| granted(c, h, cleanup, End::CancelledCleanup));
}

#[test]
fn close_before_a_grant_does_not_fabricate_a_lease_or_send_a_staged_request() {
    for requesting in [false, true] {
        run3(|c, h, cleanup| async move {
            let (mut host, mut viewer) = Box::pin(bootstrap(&c, &h)).await;
            let caps = Capabilities::default().with(Capability::Keys);
            let target = host.control_target(caps).unwrap();
            let source = host.control();
            let seat = Seat::default();
            let mut nonce = 200_000;
            let mut requested = false;
            let mut closed = false;
            let host_saw_request = Cell::new(false);
            let (report, result) = Box::pin(support::both(
                host.serve_managed_control_with_cleanup(
                    &cleanup,
                    seat.clone(),
                    |state| {
                        if let ManagedHostControlState::Pending(pending) = state {
                            host_saw_request
                                .set(host_saw_request.get() || pending.request().is_some());
                        }
                        Ok(Some(target))
                    },
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    || panic!("no granted input ticket"),
                ),
                async {
                    let result = viewer
                        .serve_interactive_control_until(
                            &cleanup,
                            402,
                            caps,
                            fr_client::input::Policy::default(),
                            |state, _| {
                                assert!(!closed);
                                match state {
                                    InteractiveViewerState::Viewing(viewing) => {
                                        if requesting {
                                            viewing.request_control().unwrap();
                                            requested = true;
                                            return Ok(ControlFlow::Continue(()));
                                        }
                                        // A staged request in this same turn must
                                        // not promote after Break cancels intent.
                                        viewing.request_control().unwrap();
                                    }
                                    InteractiveViewerState::Requesting(_) => assert!(requested),
                                    InteractiveViewerState::Controlled(_) => {
                                        panic!("no local host approval")
                                    }
                                }
                                closed = true;
                                Ok(ControlFlow::Break(closure::Reason::Requested))
                            },
                            |_| panic!("no input result without a grant"),
                        )
                        .await;
                    source.revoke();
                    result
                },
            ))
            .await;
            assert!(closed);
            if !requesting {
                assert!(!host_saw_request.get());
            }
            assert_eq!(
                result,
                Err(ObserverError::Streaming(StreamingViewerError::Closed))
            );
            assert_eq!(viewer.control_disconnect_outcome(), None);
            assert_eq!(viewer.pending_control_actions(), 0);
            assert!(report.input.is_none());
            assert_eq!(report.revocation, None);
            assert!(!seat.is_occupied());
            reap(&mut host, &mut viewer, &cleanup).await;
        });
    }
}
#[test]
fn unpolled_public_interactive_close_service_has_no_callbacks_or_completed_exchange() {
    run3(|c, h, cleanup| async move {
        let (mut host, mut viewer) = Box::pin(bootstrap(&c, &h)).await;
        let caps = Capabilities::default().with(Capability::Keys);
        let pid = viewer.worker_id();
        let future = viewer.serve_interactive_control_until(
            &cleanup,
            403,
            caps,
            fr_client::input::Policy::default(),
            |_, _| panic!("unpolled UI"),
            |_| panic!("unpolled receipt"),
        );
        drop(future);
        assert!(c.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        assert_eq!(viewer.control_disconnect_outcome(), None);
        assert_eq!(viewer.pending_control_actions(), 0);
        assert_eq!(viewer.worker_id(), pid);
        assert_eq!(
            viewer.budget_usage(),
            fr_media::delivery::BudgetUsage::default()
        );
        reap(&mut host, &mut viewer, &cleanup).await;
    });
}
