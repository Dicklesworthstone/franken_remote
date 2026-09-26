//! Real TLS/UDP against the original bounded connection windows. Session and
//! cleanup reports below are fixtures, not native effect/cleanup qualification.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;
use asupersync::cx::Cx;
use fr_core::{ids::RemoteSessionId, limits::ProtocolLimits};
use fr_transport::quic::{
    ALPN, ControlRoutes, Disposition, Error, Messages, Policy, Priority, QuicRecords, Route,
    StreamRoute,
};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    closure::{self, Cleanup, CloseRequest, Closed, ClosedReason, OutstandingEffects, Reason},
    input::{InputDelivery, InputDirection},
};
use std::{cell::Cell, time::Duration};
use support::{both, clock, runtime};

struct Pair {
    client: QuicRecords,
    host: QuicRecords,
    routes: ControlRoutes,
}
async fn pair(cx: &Cx) -> Pair {
    let (client, host) = support::native_pair_with_windows(cx, "localhost", ALPN, 1024, 2048).await;
    let (mut client, mut host) = (client.unwrap(), host.unwrap());
    let outbound = StreamRoute {
        stream: client.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        maximum: 1024,
        outbound: true,
    };
    let inbound = StreamRoute {
        stream: host.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..outbound
    };
    let routes = ControlRoutes { inbound, outbound };
    let policy = Policy {
        stream_window: 1024,
        connection_window: 2048,
        critical_connection_credit: 256,
        critical_send_bytes: 4096,
        ..Policy::default()
    };
    Pair {
        client: QuicRecords::new(client, cx, &[outbound, inbound], &[], policy).unwrap(),
        host: QuicRecords::new(
            host,
            cx,
            &[outbound, inbound].map(|r| StreamRoute {
                outbound: !r.outbound,
                ..r
            }),
            &[],
            policy,
        )
        .unwrap(),
        routes,
    }
}
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(13),
    }
}
fn stale_challenge() -> Vec<u8> {
    let message = Message::Challenge {
        scope: Scope::Observation,
        nonce: 42,
        deadline_micros: 1,
    };
    let mut bytes = vec![0; 1024];
    let n = authority::encode(
        message,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    // One valid optional extension brings this old control record to the
    // negotiated maximum; no allocation/window bound is raised for the test.
    bytes[12..16].copy_from_slice(&1000_u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&u32::try_from(1024 - n).unwrap().to_be_bytes());
    bytes[n..n + 2].copy_from_slice(&1_u16.to_be_bytes());
    bytes[n + 4..n + 8].copy_from_slice(&u32::try_from(1024 - n - 8).unwrap().to_be_bytes());
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            InputDirection::HostToViewer,
            InputDelivery::Reliable
        ),
        Ok(message)
    );
    bytes
}
fn closed_bytes() -> (Closed, Vec<u8>) {
    let report = Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    };
    let mut bytes = vec![0; closure::CLOSED_BYTES];
    closure::encode_closed(
        report,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    (report, bytes)
}
async fn consume_history(
    p: &mut Pair,
    cx: &Cx,
    stale: &[u8],
    outgoing: Route,
    prior: usize,
) -> usize {
    let mut historical = 0;
    for _ in 0..prior {
        p.host
            .send(cx, outgoing, stale, clock(cx) + 1_000_000, || true)
            .unwrap();
        let expected = historical + 1;
        while historical < expected || p.host.usage().retained_send_records != 0 {
            p.host
                .drive(cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            p.client
                .drive(cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            p.client
                .receive(
                    cx,
                    || true,
                    |_, _| {
                        historical += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
    }
    historical
}
async fn run(prior: usize) {
    let cx = Cx::current().unwrap();
    let mut p = pair(&cx).await;
    let stale = stale_challenge();
    let outgoing = Route::Stream(StreamRoute {
        outbound: true,
        ..p.routes.inbound
    });
    let historical = consume_history(&mut p, &cx, &stale, outgoing, prior).await;
    let until = clock(&cx) + 200_000;
    let closing = p.client.close_with_request(
        &cx,
        &p.client.binding(),
        p.routes,
        binding(),
        CloseRequest {
            reason: Reason::Requested,
        },
        until,
    );
    assert!(p.client.is_closed());
    let done = Cell::new(false);
    let (report, terminal) = closed_bytes();
    let mut requests = 0;
    let mut offered = 0;
    let (outcome, ()) = Box::pin(both(
        async {
            let result = closing.await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                if offered < 9 {
                    let bytes = if offered == 8 { &terminal } else { &stale };
                    match p.host.send(&cx, outgoing, bytes, until + 100_000, || true) {
                        Ok(()) => offered += 1,
                        Err(Error::Backpressure) => {}
                        error => panic!("unexpected send: {error:?}"),
                    }
                }
                p.host
                    .drive(&cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                p.host
                    .receive(
                        &cx,
                        || true,
                        |_, bytes| {
                            assert_eq!(
                                closure::decode_request(
                                    bytes,
                                    binding(),
                                    &ProtocolLimits::ABSOLUTE,
                                    InputDirection::ViewerToHost,
                                    InputDelivery::Reliable
                                ),
                                Ok(CloseRequest {
                                    reason: Reason::Requested
                                })
                            );
                            requests += 1;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                asupersync::runtime::yield_now().await;
            }
        },
    ))
    .await;
    assert_eq!(
        outcome.report,
        Some(report),
        "final report blocked after old control bytes: {outcome:?}"
    );
    assert_eq!(outcome.transport, Ok(()));
    assert_eq!(requests, 1, "no response to discarded renewal challenges");
    assert_eq!(offered, 9);
    assert_eq!(historical, prior);
}
#[test]
fn closing_consumption_advances_only_the_original_connection_window() {
    runtime().block_on(run(0));
}
#[test]
fn closing_credit_retains_preexisting_consumption_instead_of_resetting_offsets() {
    runtime().block_on(run(8));
}
