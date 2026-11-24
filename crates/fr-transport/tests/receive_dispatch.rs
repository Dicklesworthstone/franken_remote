//! Original receive authority at each callback boundary over actual UDP/TLS.
//! Payloads are framing fixtures, not native input/media or tailnet evidence.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::{cx::Cx, types::CancelKind};
use fr_transport::quic::{
    ALPN, DatagramReceipt, DatagramRoute, Disposition, Error, Policy, QuicRecords, Route,
    StreamRoute,
};
use fr_wire::HEADER_BYTES;
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{clock, drive, native_pair, pair, runtime};

#[derive(Clone, Copy, Debug)]
enum Gate {
    Caller,
    Lifetime,
    Operation,
    Cancellation,
}
const GATES: [Gate; 4] = [
    Gate::Caller,
    Gate::Lifetime,
    Gate::Operation,
    Gate::Cancellation,
];

impl Gate {
    fn install(self, q: &mut QuicRecords, cx: &Cx, live: &Arc<AtomicBool>) {
        let check = live.clone();
        match self {
            Self::Lifetime => q
                .retain_lifetime_check(cx, Arc::new(move || check.load(Ordering::Acquire)))
                .unwrap(),
            Self::Operation => q
                .retain_operation_check(cx, Arc::new(move || check.load(Ordering::Acquire)))
                .unwrap(),
            Self::Caller | Self::Cancellation => {}
        }
    }

    fn permits(self, live: &AtomicBool) -> bool {
        !matches!(self, Self::Caller) || live.load(Ordering::Acquire)
    }

    fn revoke(self, cx: &Cx, live: &AtomicBool) {
        live.store(false, Ordering::Release);
        if matches!(self, Self::Cancellation) {
            cx.cancel_fast(CancelKind::User);
        }
    }

    const fn error(self) -> Error {
        if matches!(self, Self::Cancellation) {
            Error::Cancelled
        } else {
            Error::Unauthorized
        }
    }
}

fn record(kind: u16, binding: u32, marker: u8) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_BYTES + 1];
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[6..8].copy_from_slice(&kind.to_be_bytes());
    bytes[12..16].copy_from_slice(&1_u32.to_be_bytes());
    bytes[16..20].copy_from_slice(&binding.to_be_bytes());
    bytes[HEADER_BYTES] = marker;
    bytes
}

struct Datagrams {
    client: QuicRecords,
    _server: QuicRecords,
    routes: [DatagramRoute; 2],
    receipts: [Option<DatagramReceipt>; 2],
}

async fn queued_datagrams(
    cx: &Cx,
    gate: Gate,
    live: &Arc<AtomicBool>,
    hold: bool,
    audio: bool,
) -> Datagrams {
    let (client, server) = native_pair(cx, "localhost", ALPN).await;
    let outgoing = [
        DatagramRoute {
            binding: 1,
            kind: if audio { 0x0062 } else { 0x0034 },
            outbound: true,
        },
        DatagramRoute {
            binding: 2,
            kind: 0x0034,
            outbound: true,
        },
    ];
    let routes = outgoing.map(|route| DatagramRoute {
        outbound: false,
        ..route
    });
    let mut server =
        QuicRecords::new(server.unwrap(), cx, &[], &outgoing, Policy::default()).unwrap();
    let mut client =
        QuicRecords::new(client.unwrap(), cx, &[], &routes, Policy::default()).unwrap();
    gate.install(&mut client, cx, live);
    for route in outgoing {
        server
            .send(
                cx,
                Route::Datagram(route),
                &record(
                    route.kind,
                    route.binding,
                    u8::try_from(route.binding).unwrap(),
                ),
                clock(cx) + 2_000_000,
                || true,
            )
            .unwrap();
    }
    let mut receipts = [None; 2];
    for _ in 0..4 {
        server.drive(cx, Duration::ZERO, || true).await.unwrap();
        client
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        if hold {
            client
                .receive_ready_with_receipts(
                    cx,
                    || true,
                    |_| true,
                    |route, bytes, receipt| {
                        let index = routes
                            .iter()
                            .position(|r| Route::Datagram(*r) == route)
                            .unwrap();
                        assert_eq!(bytes[HEADER_BYTES], u8::try_from(index + 1).unwrap());
                        if let Some(previous) = receipts[index] {
                            assert_eq!(receipt, Some(previous));
                        }
                        receipts[index] = receipt;
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
            if receipts.iter().all(Option::is_some) {
                break;
            }
        }
    }
    if hold {
        assert!(receipts.iter().all(Option::is_some));
        assert_eq!(client.usage().remainder_bytes, 2 * (HEADER_BYTES + 1));
    }
    Datagrams {
        client,
        _server: server,
        routes,
        receipts,
    }
}

#[test]
fn readiness_cannot_revoke_authority_then_dispatch_a_reliable_record() {
    for gate in GATES {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx, Policy::default()).await;
            let live = Arc::new(AtomicBool::new(true));
            gate.install(&mut p.client, &cx, &live);
            let send = p.host_routes[1];
            let receive = Route::Stream(StreamRoute {
                outbound: false,
                ..send
            });
            let expected = record(0x32, send.binding, 7);
            let until = clock(&cx) + 2_000_000;
            p.server
                .send(&cx, Route::Stream(send), &expected, until, || true)
                .unwrap();
            let mut held = false;
            while !held {
                assert!(clock(&cx) < until, "reliable record did not arrive");
                drive(&cx, &mut p).await;
                p.client
                    .receive_ready(
                        &cx,
                        || true,
                        |route| route == receive,
                        |_, bytes| {
                            assert_eq!(bytes, expected);
                            held = true;
                            Ok(Disposition::Blocked)
                        },
                    )
                    .unwrap();
            }
            let calls = Cell::new(0);
            let result = p.client.receive_ready(
                &cx,
                || gate.permits(&live),
                |route| {
                    if route != receive {
                        return false;
                    }
                    gate.revoke(&cx, &live);
                    true
                },
                |_, _| {
                    calls.set(calls.get() + 1);
                    Ok(Disposition::Consumed)
                },
            );
            assert_eq!(calls.get(), 0, "{gate:?} was bypassed after readiness");
            assert_eq!(result, Err(gate.error()));
            assert!(p.client.is_closed());
        });
    }
}

#[test]
fn readiness_cannot_revoke_authority_then_dispatch_a_datagram() {
    for gate in GATES {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let live = Arc::new(AtomicBool::new(true));
            let mut p = queued_datagrams(&cx, gate, &live, true, false).await;
            let calls = Cell::new(0);
            let result = p.client.receive_ready(
                &cx,
                || gate.permits(&live),
                |_| {
                    gate.revoke(&cx, &live);
                    true
                },
                |_, _| {
                    calls.set(calls.get() + 1);
                    Ok(Disposition::Consumed)
                },
            );
            assert_eq!(calls.get(), 0, "{gate:?} was bypassed after readiness");
            assert_eq!(result, Err(gate.error()));
            assert!(p.client.is_closed());
        });
    }
}

#[test]
fn a_blocked_datagram_cannot_dispatch_a_sibling_after_revocation() {
    for held in [false, true] {
        for gate in GATES {
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let live = Arc::new(AtomicBool::new(true));
                let mut p = queued_datagrams(&cx, gate, &live, held, false).await;
                let mut calls = 0;
                let result = p.client.receive(
                    &cx,
                    || gate.permits(&live),
                    |_, _| {
                        calls += 1;
                        gate.revoke(&cx, &live);
                        Ok(Disposition::Blocked)
                    },
                );
                assert_eq!(
                    calls, 1,
                    "{gate:?}, application-held={held}: a later callback escaped"
                );
                assert_eq!(result, Err(gate.error()));
                assert!(p.client.is_closed());
            });
        }
    }
}

#[test]
fn a_readiness_pause_cannot_refresh_expired_audio_or_discard_a_live_sibling() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let live = Arc::new(AtomicBool::new(true));
        let mut p = queued_datagrams(&cx, Gate::Caller, &live, true, true).await;
        let audio = Route::Datagram(p.routes[0]);
        let video = Route::Datagram(p.routes[1]);
        let receipt = p.receipts[0].unwrap();
        let paused = Cell::new(false);
        let mut received = 0;
        let count = p
            .client
            .receive_ready_with_receipts(
                &cx,
                || true,
                |route| {
                    if route == audio && !paused.replace(true) {
                        // Model the receiving thread being descheduled inside
                        // a local callback; the record keeps its original cap.
                        let left = receipt.expires_at_micros().saturating_sub(clock(&cx));
                        std::thread::sleep(Duration::from_micros(left + 1_000));
                    }
                    true
                },
                |route, bytes, original| {
                    assert_eq!(route, video, "expired audio reached its consumer");
                    assert_eq!(bytes[HEADER_BYTES], 2);
                    assert_eq!(original, p.receipts[1]);
                    received += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(
            paused.get(),
            "must cross expiry inside the readiness callback"
        );
        assert_eq!((count, received), (1, 1));
        assert_eq!(p.client.dropped_audio_datagrams(), 1);
        assert_eq!(p.client.usage().remainder_bytes, 0);
        assert!(!p.client.is_closed());
    });
}

#[test]
fn live_receive_checks_preserve_both_original_datagram_receipts() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let live = Arc::new(AtomicBool::new(true));
        let mut p = queued_datagrams(&cx, Gate::Lifetime, &live, true, false).await;
        let mut received = [0; 2];
        assert_eq!(
            p.client.receive_ready_with_receipts(
                &cx,
                || true,
                |_| true,
                |route, bytes, receipt| {
                    let index = p
                        .routes
                        .iter()
                        .position(|r| Route::Datagram(*r) == route)
                        .unwrap();
                    assert_eq!(bytes[HEADER_BYTES], u8::try_from(index + 1).unwrap());
                    assert_eq!(receipt, p.receipts[index]);
                    received[index] += 1;
                    Ok(Disposition::Consumed)
                },
            ),
            Ok(2)
        );
        assert_eq!(received, [1, 1]);
        assert_eq!(p.client.usage().remainder_bytes, 0);
        assert!(!p.client.is_closed());
    });
}
