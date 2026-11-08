//! Actual localhost UDP/TLS and the original client socket's protected close.
//! The destination guard and input fence are fixtures, not tailnet or OS-input
//! qualification. A successful send is never a remote cleanup acknowledgement.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;
#[allow(dead_code)]
#[path = "support/terminal.rs"]
mod terminal;

use asupersync::{
    cx::Cx,
    net::quic_native::NativeQuicUdpConnection,
    types::{Budget, CancelKind},
};
use fr_transport::quic::*;
use fr_wire::closure::{self, Cleanup, Closed, ClosedReason, OutstandingEffects};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{both, clock, runtime};

type Pair = terminal::Pair<NativeQuicUdpConnection>;
use crate::terminal::{
    binding, no_application_bytes, observe_close, observe_silence, queued_video as queued_datagram,
    raw_client as pair, request,
};

fn guarded(records: &mut QuicRecords, cx: &Cx) -> Arc<AtomicBool> {
    let permitted = Arc::new(AtomicBool::new(true));
    let gate = permitted.clone();
    records
        .retain_lifetime_check(cx, Arc::new(move || gate.load(Ordering::Acquire)))
        .unwrap();
    permitted
}

fn arm(records: &mut QuicRecords, cleanup: &Cx) -> (ClientAbortReport, Arc<AtomicUsize>) {
    let report = ClientAbortReport::default();
    let fenced = Arc::new(AtomicUsize::new(0));
    let count = fenced.clone();
    records
        .arm_client_abort(
            cleanup,
            &records.binding(),
            report.registration(),
            move || {
                count.fetch_add(1, Ordering::AcqRel);
            },
        )
        .unwrap();
    (report, fenced)
}

fn queued_request(p: &mut Pair, cx: &Cx, until: u64) {
    let bytes = terminal::request_bytes(closure::Reason::InspectionComplete);
    p.local
        .send(cx, Route::Stream(p.control), &bytes, until, || true)
        .unwrap();
}

fn records(p: Pair, cx: &Cx) -> (QuicRecords, QuicRecords, ControlRoutes) {
    let pair = p.into_records(cx);
    let control = pair.routes();
    (pair.local, pair.peer, control)
}

async fn original_request_then_report(
    server: &mut QuicRecords,
    cx: &Cx,
    control: ControlRoutes,
    outcome: Closed,
) -> Result<(), Error> {
    let routes = ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..control.inbound
        },
        inbound: StreamRoute {
            outbound: false,
            ..control.outbound
        },
    };
    terminal::receive_request(cx, server, routes, clock(cx) + 1_000_000).await;
    server
        .close_with_closed(cx, &server.binding(), routes.outbound, binding(), outcome)
        .await
}

#[test]
fn abrupt_close_fences_once_and_discards_both_application_and_native_queues() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        guarded(&mut p.local, &cx);
        let (report, fenced) = arm(&mut p.local, &cx);
        queued_request(&mut p, &cx, clock(&cx) + 1_000_000);
        queued_datagram(&mut p, &cx);
        assert_eq!(p.local.usage().critical_send_records, 1);
        p.local.close();
        assert!(p.local.is_closed());
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        assert_eq!(p.local.usage().retained_send_records, 0);
        p.local.close();
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        assert_eq!(p.local.tick(&cx, || true), Err(Error::Closed));
        assert_eq!(report.finish().await, Some(Ok(())));
        // Independent wire value, not the encoder under test.
        observe_close(&mut p, &cx, 0x4652_0301).await;
        no_application_bytes(&mut p);
    }));
}

#[test]
fn per_operation_authority_loss_and_application_cancellation_have_distinct_codes() {
    for cancel in [false, true] {
        let rt = runtime();
        let session = rt.request_cx_with_budget(Budget::INFINITE);
        let cleanup = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(Box::pin(async {
            let mut p = pair(&session).await;
            guarded(&mut p.local, &session);
            let (report, fenced) = arm(&mut p.local, &cleanup);
            queued_datagram(&mut p, &session);
            if cancel {
                session.cancel_fast(CancelKind::User);
            }
            assert_eq!(
                p.local.tick(&session, || false),
                Err(if cancel {
                    Error::Cancelled
                } else {
                    Error::Unauthorized
                })
            );
            assert_eq!(fenced.load(Ordering::Acquire), 1);
            assert_eq!(report.finish().await, Some(Ok(())));
            observe_close(
                &mut p,
                &cleanup,
                if cancel { 0x4652_0304 } else { 0x4652_0303 },
            )
            .await;
            no_application_bytes(&mut p);
        }));
    }
}

#[test]
fn an_expired_retained_record_notifies_the_peer_without_transmitting_the_record() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        guarded(&mut p.local, &cx);
        let (report, fenced) = arm(&mut p.local, &cx);
        queued_request(&mut p, &cx, clock(&cx) + 10_000);
        // A reliable record has a real transport delivery allowance. Exceed its
        // public upper bound rather than assuming a particular handshake RTT.
        asupersync::time::sleep(
            cx.now(),
            Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 20_000),
        )
        .await;
        assert_eq!(p.local.tick(&cx, || true), Err(Error::Expired));
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        assert_eq!(report.finish().await, Some(Ok(())));
        observe_close(&mut p, &cx, 0x4652_0302).await;
        no_application_bytes(&mut p);
    }));
}

#[test]
fn an_unacknowledged_native_prefix_does_not_prevent_the_registered_abort() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        guarded(&mut p.local, &cx);
        let (report, fenced) = arm(&mut p.local, &cx);
        queued_request(&mut p, &cx, clock(&cx) + 1_000_000);
        p.local.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert!(p.local.usage().retained_send_records > 0);
        p.local.close();
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        assert_eq!(report.finish().await, Some(Ok(())));
        observe_close(&mut p, &cx, 0x4652_0301).await;
        // The earlier drive may already have emitted the request. This proves
        // terminal delivery despite native retention, not retraction of it.
    }));
}

#[test]
fn application_peer_observes_the_authenticated_client_reason() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        guarded(&mut p.local, &cx);
        let (report, _) = arm(&mut p.local, &cx);
        p.local.close();
        assert_eq!(report.finish().await, Some(Ok(())));
        let (_, mut server, _) = records(p, &cx);
        let until = clock(&cx) + 1_000_000;
        for _ in 0..500 {
            assert!(
                clock(&cx) < until,
                "host waited for ordinary silence expiry"
            );
            match server.drive(&cx, Duration::from_millis(5), || true).await {
                Ok(()) => {}
                Err(Error::PeerClosed(PeerClose::Client(ClientCloseReason::Failure))) => {
                    assert!(server.is_closed());
                    return;
                }
                error => panic!("wrong peer-close result: {error:?}"),
            }
        }
        panic!("host did not observe the client's close");
    }));
}

#[test]
fn registration_requires_the_guard_exact_client_owner_and_a_single_consumer() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let p = pair(&cx).await;
        let (mut client, mut server, _) = records(p, &cx);
        let report = ClientAbortReport::default();
        let registration = report.registration();
        assert_eq!(
            client.arm_client_abort(&cx, &client.binding(), registration.clone(), || {}),
            Err(Error::InvalidPolicy)
        );
        guarded(&mut client, &cx);
        guarded(&mut server, &cx);
        assert_eq!(
            client.arm_client_abort(&cx, &server.binding(), registration.clone(), || {}),
            Err(Error::WrongRoute)
        );
        assert_eq!(
            server.arm_client_abort(&cx, &server.binding(), registration.clone(), || {}),
            Err(Error::WrongRoute)
        );
        let fenced = Arc::new(AtomicUsize::new(0));
        let count = fenced.clone();
        client
            .arm_client_abort(&cx, &client.binding(), registration.clone(), move || {
                count.fetch_add(1, Ordering::AcqRel);
            })
            .unwrap();
        let competing = ClientAbortReport::default();
        assert_eq!(
            client.arm_client_abort(&cx, &client.binding(), competing.registration(), || {}),
            Err(Error::InvalidPolicy)
        );
        let mut other = pair(&cx).await;
        guarded(&mut other.local, &cx);
        assert_eq!(
            other
                .local
                .arm_client_abort(&cx, &other.local.binding(), registration, || {}),
            Err(Error::InvalidPolicy)
        );
        assert_eq!(client.tick(&cx, || true), Ok(()));
        assert_eq!(fenced.load(Ordering::Acquire), 0);
        client.close();
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        drop(report);
        assert_eq!(competing.finish().await, None);
    }));
}

#[test]
fn application_traffic_and_a_dropped_registration_cannot_install_a_late_fence() {
    for datagram in [false, true] {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            guarded(&mut p.local, &cx);
            let dropped = ClientAbortReport::default();
            let registration = dropped.registration();
            drop(dropped);
            assert_eq!(
                p.local
                    .arm_client_abort(&cx, &p.local.binding(), registration, || {}),
                Err(Error::Closed)
            );
            if datagram {
                queued_datagram(&mut p, &cx);
            } else {
                queued_request(&mut p, &cx, clock(&cx) + 1_000_000);
            }
            let late = ClientAbortReport::default();
            assert_eq!(
                p.local
                    .arm_client_abort(&cx, &p.local.binding(), late.registration(), || {}),
                Err(Error::InvalidPolicy)
            );
            assert!(!p.local.is_closed());
            assert_eq!(late.finish().await, None);
            p.local.close();
            observe_silence(&mut p, &cx).await;
        }));
    }
}

#[test]
fn dropped_consumers_and_unpolled_finish_cannot_send_from_drop() {
    for abandon in 0..3 {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            guarded(&mut p.local, &cx);
            let (report, fenced) = arm(&mut p.local, &cx);
            queued_datagram(&mut p, &cx);
            match abandon {
                0 => {
                    drop(report);
                    p.local.close();
                }
                1 => {
                    p.local.close();
                    drop(report);
                }
                _ => {
                    p.local.close();
                    drop(report.finish());
                }
            }
            assert!(p.local.is_closed());
            assert_eq!(fenced.load(Ordering::Acquire), 1);
            observe_silence(&mut p, &cx).await;
        }));
    }
}

#[test]
fn delayed_finish_call_or_poll_cannot_restart_the_closure_time_budget() {
    for prepare_before_delay in [false, true] {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            guarded(&mut p.local, &cx);
            let (report, fenced) = arm(&mut p.local, &cx);
            p.local.close();
            assert_eq!(fenced.load(Ordering::Acquire), 1);
            let result = if prepare_before_delay {
                let finish = report.finish();
                asupersync::time::sleep(cx.now(), Duration::from_millis(270)).await;
                finish.await
            } else {
                asupersync::time::sleep(cx.now(), Duration::from_millis(270)).await;
                report.finish().await
            };
            assert_eq!(result, Some(Err(Error::Expired)));
            observe_silence(&mut p, &cx).await;
        }));
    }
}

#[test]
fn cleanup_cancellation_or_lost_destination_proof_suppresses_the_packet() {
    for cancel in [false, true] {
        let rt = runtime();
        let session = rt.request_cx_with_budget(Budget::INFINITE);
        let cleanup = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(Box::pin(async {
            let mut p = pair(&session).await;
            let permitted = guarded(&mut p.local, &session);
            let (report, fenced) = arm(&mut p.local, &cleanup);
            queued_datagram(&mut p, &session);
            p.local.close();
            assert_eq!(fenced.load(Ordering::Acquire), 1);
            if cancel {
                cleanup.cancel_fast(CancelKind::User);
            } else {
                permitted.store(false, Ordering::Release);
            }
            assert_eq!(
                report.finish().await,
                Some(Err(if cancel {
                    Error::Cancelled
                } else {
                    Error::Unauthorized
                }))
            );
            observe_silence(&mut p, &session).await;
        }));
    }
}

#[test]
fn destination_revocation_before_capture_never_becomes_cleanup_permission() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let permitted = guarded(&mut p.local, &cx);
        let (report, fenced) = arm(&mut p.local, &cx);
        queued_datagram(&mut p, &cx);
        permitted.store(false, Ordering::Release);
        assert_eq!(p.local.tick(&cx, || true), Err(Error::Unauthorized));
        assert_eq!(fenced.load(Ordering::Acquire), 1);
        assert_eq!(report.finish().await, Some(Err(Error::Unauthorized)));
        observe_silence(&mut p, &cx).await;
    }));
}

#[test]
fn explicit_close_request_owns_the_socket_without_an_extra_fence_or_packet() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let p = pair(&cx).await;
        let (mut client, mut server, control) = records(p, &cx);
        guarded(&mut client, &cx);
        let report = ClientAbortReport::default();
        let fence_cx = cx.clone();
        client
            .arm_client_abort(&cx, &client.binding(), report.registration(), move || {
                fence_cx.cancel_fast(CancelKind::User);
            })
            .unwrap();
        let outcome = Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Unknown,
        };
        let exchange = client.close_with_request(
            &cx,
            &client.binding(),
            control,
            binding(),
            request(),
            u64::MAX,
        );
        assert!(client.is_closed());
        assert!(
            cx.checkpoint().is_ok(),
            "fallback cancelled the original close exchange"
        );
        assert_eq!(report.finish().await, None);
        let (received, sent) = Box::pin(both(
            exchange,
            original_request_then_report(&mut server, &cx, control, outcome),
        ))
        .await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received.transport, Ok(()));
        assert!(received.request_acknowledged);
        assert_eq!(received.report, Some(outcome));
        assert!(cx.checkpoint().is_ok());
    }));
}

#[test]
fn a_host_cannot_claim_a_client_close_reason_and_the_client_does_not_echo_it() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        guarded(&mut p.local, &cx);
        let (report, fenced) = arm(&mut p.local, &cx);
        assert!(p.peer.close(&cx, 0x4652_0301).await.unwrap() > 0);
        let until = clock(&cx) + 1_000_000;
        for _ in 0..500 {
            assert!(
                clock(&cx) < until,
                "client did not receive the host's close"
            );
            match p.local.drive(&cx, Duration::from_millis(5), || true).await {
                Ok(()) => {}
                Err(Error::Closed) => {
                    assert_eq!(fenced.load(Ordering::Acquire), 1);
                    assert_eq!(report.finish().await, None);
                    return;
                }
                error => panic!("server attributed a client reason: {error:?}"),
            }
        }
        panic!("client did not observe the host's close");
    }));
}
