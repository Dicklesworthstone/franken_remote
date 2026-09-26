//! Production session dispatch and terminal transport; identity and cleanup
//! reports are explicit local fixtures, not installed-Tailscale or OS proof.
use super::*;
use crate::session_startup::HostSession;
use fr_wire::closure::{self, Cleanup, Closed, ClosedReason, OutstandingEffects};

fn with_cleanup<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = network::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(Box::pin(async move {
        asupersync::time::timeout(
            cleanup.now(),
            Duration::from_secs(6),
            Box::pin(f(c, h, cleanup)),
        )
        .await
        .unwrap();
    }));
}
async fn opened(c: &Cx, h: &Cx, role: Role) -> (HostSession, ViewerSession) {
    let mut cfg = config(false);
    cfg.offer.role = role;
    cfg.transport.critical_send_records = 4;
    let (client, server) = network::native_pair(c, "localhost", quic::ALPN).await;
    let peer = Peer::Fixture {
        alive: Arc::new(AtomicBool::new(true)),
        until: now(h).unwrap() + 30_000_000,
        control: role == Role::RequestControl,
    };
    let mut client = Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut host = Host::start(h.clone(), server.unwrap(), peer, cfg).unwrap();
    ready(&mut host, &mut client, false).await;
    let mut host = host.finish().unwrap().into_running().unwrap();
    let mut viewer = client.finish().unwrap();
    // Flush/ack only the original negotiation before entering the terminal path.
    // No report may use an absence of native backlog inferred from packet counts.
    for _ in 0..8 {
        let (a, b) = Box::pin(network::both(
            host.io()
                .unwrap()
                .0
                .drive(h, Duration::from_millis(1), || true),
            viewer.drive(Duration::from_millis(1), no_other),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    (host, viewer)
}
fn expected(reason: ClosedReason) -> Closed {
    Closed {
        reason,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
async fn receive_end(viewer: &mut ViewerSession) -> Error {
    loop {
        match viewer
            .drive(Duration::from_millis(1), |_, _| {
                panic!("terminal record escaped dispatcher")
            })
            .await
        {
            Ok(()) => {}
            Err(error) => return error,
        }
    }
}
#[test]
fn host_fences_at_call_time_and_viewer_retains_the_exact_final_report() {
    with_cleanup(|c, h, cleanup| async move {
        let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::Observe)).await;
        let control = host.observation().unwrap();
        let original_silence = viewer.heard_until;
        let report = expected(ClosedReason::HostStopping);
        let end = host.close_with_report(&cleanup, report.reason);
        assert!(control.check().is_err());
        assert!(h.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        assert!(host.io().is_err());
        let (sent, received) = Box::pin(network::both(end, receive_end(&mut viewer))).await;
        assert_eq!(received, Error::RemoteClosed(report));
        // Receipt closes the viewer immediately. Its ACK may be lost; absence
        // of that ACK never changes the cleanup/effect report already received.
        assert!(matches!(
            sent,
            Ok(()) | Err(Error::Transport(quic::Error::Expired))
        ));
        assert_eq!(viewer.closed_report(), Some(report));
        assert_eq!(viewer.check(), Err(Error::RemoteClosed(report)));
        assert_eq!(viewer.heard_until, original_silence);
        assert!(viewer.responder.response_deadline().is_none());
        assert!(c.is_cancel_requested());
        viewer.close();
        assert_eq!(viewer.closed_report(), Some(report));
    });
}
#[test]
fn completed_cleanup_and_uncertain_effects_remain_independent_peer_reports() {
    with_cleanup(|c, h, cleanup| async move {
        let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::Observe)).await;
        let control = host.observation().unwrap();
        let bound = host.binding();
        let report = Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Known {
                pending: 2,
                uncertain: 3,
            },
        };
        let (transport, routes) = host.io().unwrap();
        let original = transport.binding();
        control.revoke();
        let end = transport.close_with_closed(
            &cleanup,
            &original,
            routes.outbound,
            authority::Binding {
                channel: bound.id,
                session: bound.remote_session,
            },
            report,
        );
        let (_, received) = Box::pin(network::both(end, receive_end(&mut viewer))).await;
        assert_eq!(received, Error::RemoteClosed(report));
        assert_eq!(viewer.closed_report(), Some(report));
        assert_eq!(
            viewer.closed_report().unwrap().effects,
            OutstandingEffects::Known {
                pending: 2,
                uncertain: 3
            }
        );
    });
}
#[test]
fn queued_application_records_after_closed_never_dispatch_or_renew_the_viewer() {
    with_cleanup(|c, h, _cleanup| async move {
        let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::Observe)).await;
        let report = expected(ClosedReason::PermissionLost);
        let binding = authority::Binding {
            channel: host.binding().id,
            session: host.binding().remote_session,
        };
        let original_silence = viewer.heard_until;
        let mut bytes = [0; closure::CLOSED_BYTES];
        closure::encode_closed(
            report,
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        // Deliberately adversarial peer: queue more application data after its
        // final report. This tests the receiver, not honest host cleanup.
        let (transport, routes) = host.io().unwrap();
        let until = now(&h).unwrap() + 1_000_000;
        for _ in 0..2 {
            transport
                .send(&h, Route::Stream(routes.outbound), &bytes, until, || true)
                .unwrap();
        }
        let mut tail = [0; closure::REQUEST_BYTES];
        closure::encode_request(
            closure::CloseRequest {
                reason: closure::Reason::Requested,
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut tail,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        transport
            .send(&h, Route::Stream(routes.outbound), &tail, until, || true)
            .unwrap();
        let received = loop {
            let (sent, received) = Box::pin(network::both(
                transport.drive(&h, Duration::from_millis(1), || true),
                viewer.drive(Duration::from_millis(1), |_, _| {
                    panic!("record dispatched after final report")
                }),
            ))
            .await;
            sent.unwrap();
            if let Err(error) = received {
                break error;
            }
            assert!(now(&h).unwrap() < until);
        };
        assert_eq!(received, Error::RemoteClosed(report));
        assert_eq!(viewer.heard_until, original_silence);
        assert_eq!(
            viewer.tick(|_, _| panic!("resumed after close")),
            Err(Error::RemoteClosed(report))
        );
    });
}
#[test]
fn wrong_session_or_noncanonical_report_is_terminal_without_a_success_record() {
    for malformed in [false, true] {
        with_cleanup(move |c, h, _cleanup| async move {
            let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::Observe)).await;
            let binding = authority::Binding {
                channel: host.binding().id,
                session: host.binding().remote_session,
            };
            let mut bytes = [0; closure::CLOSED_BYTES];
            closure::encode_closed(
                expected(ClosedReason::HostFailure),
                binding,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            if malformed {
                bytes[51] = 1;
            } else {
                bytes[39] ^= 1;
            }
            let (transport, routes) = host.io().unwrap();
            transport
                .send(
                    &h,
                    Route::Stream(routes.outbound),
                    &bytes,
                    now(&h).unwrap() + 1_000_000,
                    || true,
                )
                .unwrap();
            loop {
                let (a, b) = Box::pin(network::both(
                    transport.drive(&h, Duration::from_millis(1), || true),
                    viewer.drive(Duration::from_millis(1), |_, _| {
                        panic!("malformed close escaped")
                    }),
                ))
                .await;
                a.unwrap();
                if let Err(error) = b {
                    assert_eq!(error, Error::Transport(quic::Error::Malformed));
                    break;
                }
            }
            assert!(viewer.is_closed());
            assert_eq!(viewer.closed_report(), None);
            assert!(c.is_cancel_requested());
        });
    }
}
#[test]
fn absent_report_unpolled_shutdown_and_control_intent_do_not_invent_completion() {
    with_cleanup(|c, h, cleanup| async move {
        let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::Observe)).await;
        let control = host.observation().unwrap();
        drop(host.close_with_report(&cleanup, ClosedReason::HostStopping));
        assert!(control.check().is_err());
        assert!(h.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        viewer.close();
        assert_eq!(viewer.closed_report(), None);
        assert_eq!(viewer.check(), Err(Error::Closed));
    });
    with_cleanup(|c, h, cleanup| async move {
        let (mut host, mut viewer) = Box::pin(opened(&c, &h, Role::RequestControl)).await;
        let control = host.observation().unwrap();
        let end = host.close_with_report(&cleanup, ClosedReason::HostStopping);
        assert!(control.check().is_err());
        assert_eq!(Box::pin(end).await, Err(Error::Order));
        viewer.close();
        assert_eq!(viewer.closed_report(), None);
    });
}
