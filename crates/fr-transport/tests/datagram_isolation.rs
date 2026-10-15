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
        client.receive(
            &cx,
            || true,
            |r, bytes| {
                assert_eq!(r, Route::Datagram(incoming[0]));
                assert_eq!(bytes, audio);
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
