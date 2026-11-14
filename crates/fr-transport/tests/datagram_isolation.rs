//! Route isolation over real authenticated UDP/TLS, not a simulated endpoint.
//! Payloads are framing fixtures; these tests make no codec/tailnet claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)] // The shared TLS fixture also exposes helpers for other tests.
mod support;

use asupersync::cx::Cx;
use fr_transport::quic::{ALPN, DatagramRoute, Disposition, Error, Policy, QuicRecords, Route};
use fr_wire::HEADER_BYTES;
use std::time::Duration;
use support::{clock, native_pair, runtime};

fn record(route: DatagramRoute, marker: u8) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_BYTES + 1];
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[6..8].copy_from_slice(&route.kind.to_be_bytes());
    bytes[12..16].copy_from_slice(&1_u32.to_be_bytes());
    bytes[16..20].copy_from_slice(&route.binding.to_be_bytes());
    bytes[HEADER_BYTES] = marker;
    bytes
}

#[allow(clippy::too_many_lines)]
async fn isolate_blocked_route(through_readiness: bool) {
    let cx = Cx::current().unwrap();
    let (client, server) = native_pair(&cx, "localhost", ALPN).await;
    let outgoing = [
        DatagramRoute {
            binding: 1,
            kind: 0x0062,
            outbound: true,
        },
        DatagramRoute {
            binding: 2,
            kind: 0x0034,
            outbound: true,
        },
    ];
    let incoming = outgoing.map(|r| DatagramRoute {
        outbound: false,
        ..r
    });
    let mut server =
        QuicRecords::new(server.unwrap(), &cx, &[], &outgoing, Policy::default()).unwrap();
    let mut client =
        QuicRecords::new(client.unwrap(), &cx, &[], &incoming, Policy::default()).unwrap();
    let audio = record(outgoing[0], 17);
    let video = record(outgoing[1], 23);
    server
        .send(
            &cx,
            Route::Datagram(outgoing[0]),
            &audio,
            clock(&cx) + 2_000_000,
            || true,
        )
        .unwrap();

    // Establish blocked custody before sending the second route. This avoids
    // depending on UDP arrival order and plants the exact old single-slot bug.
    for _ in 0..64 {
        server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        client
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        client
            .receive_ready(
                &cx,
                || true,
                |_| !through_readiness,
                |r, bytes| {
                    assert_eq!(r, Route::Datagram(incoming[0]));
                    assert_eq!(bytes, audio);
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        if client.usage().remainder_bytes == audio.len() {
            break;
        }
    }
    assert_eq!(client.usage().remainder_bytes, audio.len());
    let mut original_receipt = None;
    assert_eq!(
        client.receive_ready_with_receipts(
            &cx,
            || true,
            |_| true,
            |r, bytes, receipt| {
                assert_eq!(r, Route::Datagram(incoming[0]));
                assert_eq!(bytes, audio);
                let receipt = receipt.expect("original datagram custody");
                assert!(receipt.received_at_micros() <= clock(&cx));
                assert_eq!(
                    receipt.expires_at_micros() - receipt.received_at_micros(),
                    100_000,
                    "audio's original ceiling includes transport backpressure"
                );
                original_receipt = Some(receipt);
                Ok(Disposition::Blocked)
            }
        ),
        Ok(0)
    );
    assert!(original_receipt.is_some());
    server
        .send(
            &cx,
            Route::Datagram(outgoing[1]),
            &video,
            clock(&cx) + 2_000_000,
            || true,
        )
        .unwrap();
    let mut video_received = 0;
    for _ in 0..64 {
        server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        client
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        client
            .receive_ready(
                &cx,
                || true,
                |r| !through_readiness || r != Route::Datagram(incoming[0]),
                |r, bytes| {
                    if r == Route::Datagram(incoming[0]) {
                        assert!(!through_readiness);
                        assert_eq!(bytes, audio);
                        return Ok(Disposition::Blocked);
                    }
                    assert_eq!(r, Route::Datagram(incoming[1]));
                    assert_eq!(bytes, video);
                    video_received += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if video_received == 1 {
            break;
        }
    }
    assert_eq!(
        video_received, 1,
        "blocked audio held back another datagram route"
    );
    assert_eq!(client.usage().remainder_bytes, audio.len());
    assert!(!client.is_closed());

    // Becoming ready consumes exactly the original held audio, not a retry,
    // replacement, or new authority grant.
    assert_eq!(
        client.receive_ready_with_receipts(
            &cx,
            || true,
            |_| true,
            |r, bytes, receipt| {
                assert_eq!(r, Route::Datagram(incoming[0]));
                assert_eq!(bytes, audio);
                assert_eq!(receipt, original_receipt, "no new lifetime at delivery");
                Ok(Disposition::Consumed)
            }
        ),
        Ok(1)
    );
    assert_eq!(client.usage().remainder_bytes, 0);
    assert_eq!(
        client.receive(&cx, || false, |_, _| panic!("revoked")),
        Err(Error::Unauthorized)
    );
    assert!(client.is_closed());
    assert_eq!(client.usage().remainder_bytes, 0);
}

#[test]
fn readiness_backpressure_is_isolated_between_native_datagram_routes() {
    runtime().block_on(isolate_blocked_route(true));
}

#[test]
fn handler_backpressure_is_isolated_between_native_datagram_routes() {
    runtime().block_on(isolate_blocked_route(false));
}

/// A receive that blocks a route it does not own (the controlled viewer's
/// clock-only pass, a startup handshake) must not cost that route its later
/// datagrams. With per-route custody that dropped a full route's arrivals,
/// one such pass held the first video fragment and discarded the rest of the
/// turn: on a clean link every controlled namespace session then ended
/// `video_reference_lost` (12 of 79 suite tests).
#[test]
fn a_pass_that_blocks_a_route_loses_none_of_its_later_datagrams() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (client, server) = native_pair(&cx, "localhost", ALPN).await;
        let outgoing = [DatagramRoute {
            binding: 2,
            kind: 0x0034,
            outbound: true,
        }];
        let incoming = outgoing.map(|r| DatagramRoute {
            outbound: false,
            ..r
        });
        let mut server =
            QuicRecords::new(server.unwrap(), &cx, &[], &outgoing, Policy::default()).unwrap();
        let mut client =
            QuicRecords::new(client.unwrap(), &cx, &[], &incoming, Policy::default()).unwrap();
        let sent: Vec<_> = (0..6).map(|n| record(outgoing[0], 40 + n)).collect();
        for bytes in &sent {
            server
                .send(
                    &cx,
                    Route::Datagram(outgoing[0]),
                    bytes,
                    clock(&cx) + 2_000_000,
                    || true,
                )
                .unwrap();
        }
        let mut received = Vec::new();
        for _ in 0..64 {
            server.drive(&cx, Duration::ZERO, || true).await.unwrap();
            client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            // First a pass that owns nothing here, then the real consumer.
            client
                .receive(&cx, || true, |_, _| Ok(Disposition::Blocked))
                .unwrap();
            client
                .receive(
                    &cx,
                    || true,
                    |r, bytes| {
                        assert_eq!(r, Route::Datagram(incoming[0]));
                        received.push(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received.len() == sent.len() {
                break;
            }
        }
        assert_eq!(received, sent, "a blocking pass lost datagrams");
    });
}

/// After a pass that blocks a route it does not own, the route's consumer
/// drains everything already queued in ONE receive turn, not one datagram per
/// turn. The controlled viewer runs such a pass (its clock-only receive)
/// before every media receive. With one datagram per turn, a 60 fps picture
/// of 15-20 fragments fell further behind each frame. The viewer asked for
/// whole pictures again, the late originals arrived as duplicates, and live
/// control sessions against a Windows host ended `RecoveryLimitExceeded` or
/// `video_reference_lost` on a link that lost nothing.
#[test]
fn a_blocking_pass_leaves_the_route_draining_at_full_turn_rate() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (client, server) = native_pair(&cx, "localhost", ALPN).await;
        let outgoing = [DatagramRoute {
            binding: 2,
            kind: 0x0034,
            outbound: true,
        }];
        let incoming = outgoing.map(|r| DatagramRoute {
            outbound: false,
            ..r
        });
        let mut server =
            QuicRecords::new(server.unwrap(), &cx, &[], &outgoing, Policy::default()).unwrap();
        let mut client =
            QuicRecords::new(client.unwrap(), &cx, &[], &incoming, Policy::default()).unwrap();
        // Fewer than one turn's record budget, so a single turn can take all.
        let sent: Vec<_> = (0..10).map(|n| record(outgoing[0], 60 + n)).collect();
        for bytes in &sent {
            // The sender's own window refuses a burst: flush it (the client
            // only drives, it receives nothing yet) and retry.
            let mut tries = 0;
            while let Err(error) = server.send(
                &cx,
                Route::Datagram(outgoing[0]),
                bytes,
                clock(&cx) + 2_000_000,
                || true,
            ) {
                assert_eq!(error, Error::Backpressure);
                tries += 1;
                assert!(tries < 64, "the sender never drained its window");
                server.drive(&cx, Duration::ZERO, || true).await.unwrap();
                client
                    .drive(&cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
            }
        }
        // Every datagram reaches the client's native queue before any receive.
        for _ in 0..16 {
            server.drive(&cx, Duration::ZERO, || true).await.unwrap();
            client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
        }
        // The pass that owns nothing holds the first arrival and parks the next.
        assert_eq!(
            client.receive(&cx, || true, |_, _| Ok(Disposition::Blocked)),
            Ok(0)
        );
        let mut received = Vec::new();
        let consumed = client
            .receive(
                &cx,
                || true,
                |r, bytes| {
                    assert_eq!(r, Route::Datagram(incoming[0]));
                    received.push(bytes.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert_eq!(
            consumed,
            sent.len(),
            "one turn after a blocking pass drained {consumed} of {} queued datagrams",
            sent.len()
        );
        assert_eq!(received, sent, "in arrival order, none lost or repeated");
        assert_eq!(client.usage().remainder_bytes, 0);
    });
}
