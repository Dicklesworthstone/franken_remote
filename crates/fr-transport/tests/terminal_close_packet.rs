//! Actual localhost TLS/UDP, production registered terminal custody and native
//! `CONNECTION_CLOSE`. Admission and the local authority fence are fixtures; no
//! Tailscale, native input release or desktop qualification is claimed.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::{cx::Cx, net::quic_native::NativeQuicUdpConnection, types::CancelKind};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    FrameDescriptor, MediaLimits,
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects},
    lease_revoked::Reason,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{clock, runtime};

struct Pair {
    client: NativeQuicUdpConnection,
    server: QuicRecords,
    control: StreamRoute,
    incoming: StreamRoute,
    bulk: StreamRoute,
    video: DatagramRoute,
}
async fn pair(cx: &Cx) -> Pair {
    let (client, server) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    let control = StreamRoute {
        stream: server.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 1150,
    };
    let incoming = StreamRoute {
        stream: client.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..control
    };
    let bulk = StreamRoute {
        stream: server.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 8,
        messages: Messages::Exact(0x0032),
        priority: Priority::Bulk,
        outbound: true,
        maximum: 65536,
    };
    let video = DatagramRoute {
        binding: 9,
        kind: 0x0034,
        outbound: true,
    };
    let server = QuicRecords::new(
        server,
        cx,
        &[control, incoming, bulk],
        &[video],
        Policy::default(),
    )
    .unwrap();
    Pair {
        client,
        server,
        control,
        incoming,
        bulk,
        video,
    }
}
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(1),
    }
}
fn arm(pair: &mut Pair, cx: &Cx) -> (RevocationReport, Arc<AtomicBool>) {
    let report = RevocationReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let signal = fenced.clone();
    pair.server
        .arm_revocation_report(
            cx,
            &pair.server.binding(),
            pair.control,
            binding(),
            InputLeaseId::from_raw(2),
            report.registration(),
            move || {
                signal.store(true, Ordering::Release);
                Reason::LocalRevoke
            },
        )
        .unwrap();
    (report, fenced)
}
fn queued_video(pair: &mut Pair, cx: &Cx) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let mut bytes = [0; 1150];
    let n = fr_wire::encode_fragment(
        fr_wire::Fragment {
            descriptor: FrameDescriptor {
                frame: 0,
                reference: None,
                total_bytes: 4,
                stride: 4,
                capture_micros: 1,
            },
            index: 0,
            bytes: b"data",
        },
        9,
        &limits,
        &mut bytes,
    )
    .unwrap();
    // This queues inside the native DATAGRAM owner without flushing a packet.
    pair.server
        .send(
            cx,
            Route::Datagram(pair.video),
            &bytes[..n],
            clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
}
async fn observe_close(pair: &mut Pair, cx: &Cx, code: u64) {
    let until = clock(cx) + 1_000_000;
    while !pair.client.connection().close_was_peer_initiated() {
        assert!(clock(cx) < until, "peer received no protected close");
        let result = pair
            .client
            .drive_io_once(cx, Duration::from_millis(5))
            .await;
        assert!(result.is_ok(), "native receive: {result:?}");
        assert!(
            pair.client.connection_mut().recv_datagram().is_none(),
            "native queued video escaped after the authority fence"
        );
    }
    assert_eq!(
        pair.client.connection().inner().transport().close_code(),
        Some(code)
    );
}
async fn observe_silence(pair: &mut Pair, cx: &Cx) {
    pair.client
        .drive_io_once(cx, Duration::from_millis(10))
        .await
        .unwrap();
    assert!(!pair.client.connection().close_was_peer_initiated());
    assert!(pair.client.connection_mut().recv_datagram().is_none());
}

#[test]
fn authority_failure_with_native_video_sends_only_a_close_not_a_successful_report() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut pair = Box::pin(pair(&cx)).await;
        let (report, fenced) = arm(&mut pair, &cx);
        queued_video(&mut pair, &cx);
        assert_eq!(pair.server.tick(&cx, || false), Err(Error::Unauthorized));
        assert!(fenced.load(Ordering::Acquire));
        assert!(pair.server.is_closed());
        assert_eq!(report.finish().await, Some(Err(Error::Backpressure)));
        // Independent numeric wire value: revoked namespace + LocalRevoke(1).
        observe_close(&mut pair, &cx, 0x4652_0201).await;
        let streams = pair.client.connection().inner().streams();
        if let Ok(stream) = streams.stream(pair.bulk.stream) {
            assert_eq!(stream.recv_offset, 0);
        }
    }));
}

#[test]
fn the_application_peer_ends_on_close_instead_of_waiting_for_silence_expiry() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut pair = Box::pin(pair(&cx)).await;
        let (report, _) = arm(&mut pair, &cx);
        let routes = [pair.control, pair.incoming, pair.bulk].map(|route| StreamRoute {
            outbound: !route.outbound,
            ..route
        });
        let video = DatagramRoute {
            outbound: false,
            ..pair.video
        };
        queued_video(&mut pair, &cx);
        pair.server.close();
        assert_eq!(report.finish().await, Some(Err(Error::Backpressure)));
        let mut client =
            QuicRecords::new(pair.client, &cx, &routes, &[video], Policy::default()).unwrap();
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(
                clock(&cx) < until,
                "application peer waited for silence expiry"
            );
            match client.drive(&cx, Duration::from_millis(5), || true).await {
                Ok(()) => {}
                // The authenticated reason, not a bare transport closure.
                Err(Error::PeerClosed(PeerClose::Lease(Reason::LocalRevoke))) => break,
                error => panic!("unexpected application close result: {error:?}"),
            }
        }
        assert!(client.is_closed());
    }));
}

#[test]
fn deferred_closed_uses_the_final_reason_not_the_construction_placeholder() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut pair = Box::pin(pair(&cx)).await;
        let reporting = ClosedReport::default();
        let fenced = Arc::new(AtomicBool::new(false));
        let signal = fenced.clone();
        pair.server
            .arm_closed_report(
                &cx,
                &pair.server.binding(),
                pair.control,
                binding(),
                reporting.registration(),
                move || signal.store(true, Ordering::Release),
            )
            .unwrap();
        queued_video(&mut pair, &cx);
        pair.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert!(pair.server.is_closed());
        let result = reporting
            .finish(Closed {
                reason: ClosedReason::HostStopping,
                cleanup: Cleanup::Unconfirmed,
                effects: OutstandingEffects::Unknown,
            })
            .await;
        assert_eq!(result, Some(Err(Error::Backpressure)));
        // The placeholder is HostFailure(7); final HostStopping(2) must win.
        observe_close(&mut pair, &cx, 0x4652_0102).await;
    }));
}

#[test]
fn cleanup_security_loss_or_cancellation_suppresses_the_fallback_packet() {
    for cancel in [false, true] {
        let rt = runtime();
        let session = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        rt.block_on(Box::pin(async {
            let mut pair = Box::pin(pair(&session)).await;
            let permitted = Arc::new(AtomicBool::new(true));
            let check = permitted.clone();
            pair.server
                .retain_lifetime_check(&session, Arc::new(move || check.load(Ordering::Acquire)))
                .unwrap();
            let (reporting, fenced) = arm(&mut pair, &cleanup);
            queued_video(&mut pair, &session);
            pair.server.close();
            assert!(fenced.load(Ordering::Acquire));
            if cancel {
                cleanup.cancel_fast(CancelKind::User);
            } else {
                permitted.store(false, Ordering::Release);
            }
            assert_eq!(
                reporting.finish().await,
                Some(Err(if cancel {
                    Error::Cancelled
                } else {
                    Error::Unauthorized
                }))
            );
            observe_silence(&mut pair, &session).await;
        }));
    }
}

#[test]
fn delayed_or_abandoned_fallback_does_not_send_or_restart_the_original_budget() {
    for delay in [false, true] {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut pair = Box::pin(pair(&cx)).await;
            let (reporting, fenced) = arm(&mut pair, &cx);
            queued_video(&mut pair, &cx);
            pair.server.close();
            assert!(fenced.load(Ordering::Acquire));
            let finish = reporting.finish();
            if delay {
                asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
                assert_eq!(finish.await, Some(Err(Error::Expired)));
            } else {
                drop(finish);
            }
            assert!(pair.server.is_closed());
            observe_silence(&mut pair, &cx).await;
        }));
    }
}

#[test]
fn unacknowledged_stream_backlog_still_allows_the_registered_close_packet() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut pair = Box::pin(pair(&cx)).await;
        let (reporting, fenced) = arm(&mut pair, &cx);
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
        let mut bytes = [0; 8192];
        let data = [99; 8000];
        let n = fr_wire::encode_recovery(
            fr_wire::RecoveryChunk {
                frame: 0,
                total_bytes: 8000,
                offset: 0,
                capture_micros: 1,
                bytes: &data,
            },
            8,
            &limits,
            &mut bytes,
        )
        .unwrap();
        pair.server
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bytes[..n],
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        pair.server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        assert!(pair.server.usage().retained_send_records > 0);
        pair.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
        // Earlier packets may already be in flight. No claim of retracting them.
        observe_close(&mut pair, &cx, 0x4652_0201).await;
    }));
}
