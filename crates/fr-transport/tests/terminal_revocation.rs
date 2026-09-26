//! Real UDP/TLS/QUIC terminal reporting. The grant/fence is a local fixture;
//! these tests make no native-input, Tailscale, or cleanup-success claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    MediaLimits, RecoveryChunk,
    authority::Binding,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, REVOKED_BYTES, Reason, Revoked},
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{both, clock, runtime};

struct Pair {
    client: QuicRecords,
    server: QuicRecords,
    control: StreamRoute,
    bulk: StreamRoute,
    video: DatagramRoute,
}
async fn pair(cx: &Cx) -> Pair {
    let (c, s) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut c, mut s) = (c.unwrap(), s.unwrap());
    let control = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 1150,
    };
    let bulk = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 8,
        messages: Messages::Exact(0x0032),
        priority: Priority::Bulk,
        outbound: true,
        maximum: 65536,
    };
    let incoming = StreamRoute {
        stream: c.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..control
    };
    let routes = [control, bulk, incoming];
    let video = DatagramRoute {
        binding: 9,
        kind: 0x0034,
        outbound: true,
    };
    let client = QuicRecords::new(
        c,
        cx,
        &routes.map(|r| StreamRoute {
            outbound: !r.outbound,
            ..r
        }),
        &[DatagramRoute {
            outbound: false,
            ..video
        }],
        Policy::default(),
    )
    .unwrap();
    let server = QuicRecords::new(s, cx, &routes, &[video], Policy::default()).unwrap();
    Pair {
        client,
        server,
        control,
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
fn report() -> Revoked {
    Revoked {
        lease: InputLeaseId::from_raw(2),
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    }
}
fn bulk() -> Vec<u8> {
    let mut bytes = vec![0; 65536];
    let data = vec![99; 8000];
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let n = fr_wire::encode_recovery(
        RecoveryChunk {
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
    bytes.truncate(n);
    bytes
}

#[test]
fn terminal_report_crosses_real_quic_but_unstaged_media_does_not() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        pair.server
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(pair.server.usage().retained_send_records, 1);
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(
            pair.server.is_closed(),
            "ordinary traffic must stop before polling"
        );
        assert_eq!(pair.server.usage().retained_send_records, 0);
        let done = Cell::new(false);
        let mut received = Vec::new();
        let (ended, ()) = Box::pin(both(
            async {
                let ended = end.await;
                done.set(true);
                ended
            },
            async {
                while !done.get() {
                    pair.client
                        .drive(&cx, Duration::from_millis(1), || true)
                        .await
                        .unwrap();
                    pair.client
                        .receive(
                            &cx,
                            || true,
                            |route, bytes| {
                                assert_eq!(
                                    route,
                                    Route::Stream(StreamRoute {
                                        outbound: false,
                                        ..pair.control
                                    })
                                );
                                assert_eq!(bytes.len(), REVOKED_BYTES);
                                received.push(
                                    lease_revoked::decode(
                                        bytes,
                                        binding(),
                                        report().lease,
                                        &ProtocolLimits::ABSOLUTE,
                                        InputDirection::HostToViewer,
                                        InputDelivery::Reliable,
                                    )
                                    .unwrap(),
                                );
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                }
            },
        ))
        .await;
        assert_eq!(ended, Ok(()));
        assert_eq!(received, [report()]);
        assert_eq!(
            pair.server.send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::Closed)
        );
    });
}

#[test]
fn dropping_an_unpolled_report_cannot_leave_the_original_owner_live() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.server.is_closed());
        drop(end);
        assert_eq!(pair.server.tick(&cx, || true), Err(Error::Closed));
        assert_eq!(
            pair.client
                .receive(&cx, || true, |_, _| panic!("unpolled report sent")),
            Ok(0)
        );
    });
}

#[test]
fn wrong_connection_proof_does_not_close_a_replacement() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let foreign = pair.client.binding();
        assert_eq!(
            pair.server
                .close_with_revocation(&cx, &foreign, pair.control, binding(), report())
                .await,
            Err(Error::WrongRoute)
        );
        assert!(!pair.server.is_closed());
        assert_eq!(pair.server.tick(&cx, || true), Ok(()));
    });
}

#[test]
fn matched_owner_closes_on_wrong_route_and_malformed_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for malformed in [false, true] {
            let mut pair = pair(&cx).await;
            let original = pair.server.binding();
            let route = if malformed { pair.control } else { pair.bulk };
            let notice = if malformed {
                Revoked {
                    lease: InputLeaseId::from_raw(0),
                    ..report()
                }
            } else {
                report()
            };
            let end = pair
                .server
                .close_with_revocation(&cx, &original, route, binding(), notice);
            assert!(pair.server.is_closed());
            assert_eq!(
                end.await,
                Err(if malformed {
                    Error::Malformed
                } else {
                    Error::WrongRoute
                })
            );
        }
    });
}

#[test]
fn immutable_ingress_guard_and_parent_cancellation_survive_handoff() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let allowed = Arc::new(AtomicBool::new(true));
        let guard = allowed.clone();
        pair.server
            .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
            .unwrap();
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        allowed.store(false, Ordering::Release);
        assert_eq!(end.await, Err(Error::Unauthorized));
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        cx.cancel_fast(CancelKind::User);
        assert_eq!(end.await, Err(Error::Cancelled));
    });
}

#[test]
fn delayed_first_poll_does_not_restart_terminal_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
        assert_eq!(end.await, Err(Error::Expired));
    });
}

#[test]
fn native_retained_stream_payload_is_refused_instead_of_flushed_after_revocation() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        pair.server
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        pair.server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        assert!(pair.server.usage().retained_send_records > 0);
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.server.is_closed());
        assert_eq!(end.await, Err(Error::Backpressure));
    });
}

#[test]
fn native_queued_datagram_is_refused_without_a_post_fence_flush() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
        let mut bytes = [0; 1150];
        let n = fr_wire::encode_fragment(
            fr_wire::Fragment {
                descriptor: fr_wire::FrameDescriptor {
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
        pair.server
            .send(
                &cx,
                Route::Datagram(pair.video),
                &bytes[..n],
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.server.is_closed());
        assert_eq!(end.await, Err(Error::Backpressure));
        pair.client
            .drive(&cx, Duration::from_millis(10), || true)
            .await
            .unwrap();
        assert_eq!(
            pair.client
                .receive(&cx, || true, |_, _| panic!("stale datagram leaked")),
            Ok(0)
        );
    });
}

#[path = "terminal_revocation/deferred.rs"]
mod deferred;

#[path = "terminal_revocation/closed.rs"]
mod closed;

#[path = "terminal_revocation/closed_deferred.rs"]
mod closed_deferred;
