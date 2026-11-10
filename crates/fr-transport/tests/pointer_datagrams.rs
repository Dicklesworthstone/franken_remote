//! Replaceable pointer custody over actual authenticated UDP/TLS.
//! The other datagram bodies are framing fixtures, not codec/device evidence.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::{cx::Cx, time::sleep_until, types::Time};
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputCredentials, InputEvent, InputRequest, InputView},
    limits::ProtocolLimits,
};
use fr_transport::quic::{ALPN, DatagramRoute, Disposition, Error, Policy, QuicRecords, Route};
use fr_wire::{
    HEADER_BYTES,
    input::{INPUT_PREFIX_BYTES, InputDelivery, InputDirection, encode_input},
};
use std::time::Duration;
use support::{clock, native_pair, runtime};

fn pointer(route: DatagramRoute, sequence: u64) -> Vec<u8> {
    let request = InputRequest {
        credentials: InputCredentials {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        },
        sequence,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 100, y: 200 },
        },
    };
    let mut bytes = vec![0; HEADER_BYTES + INPUT_PREFIX_BYTES + 8];
    let length = encode_input(
        request,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        route.binding,
        InputDirection::ViewerToHost,
        InputDelivery::Datagram,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

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
async fn blocked_pointer_preserves_other_routes(through_readiness: bool) {
    let cx = Cx::current().unwrap();
    let (client, server) = native_pair(&cx, "localhost", ALPN).await;
    let outgoing = [
        DatagramRoute {
            binding: 1,
            kind: 0x0042,
            outbound: true,
        },
        DatagramRoute {
            binding: 2,
            kind: 0x0062,
            outbound: true,
        },
        DatagramRoute {
            binding: 3,
            kind: 0x0034,
            outbound: true,
        },
    ];
    let incoming = outgoing.map(|route| DatagramRoute {
        outbound: false,
        ..route
    });
    let policy = Policy {
        record_lifetime_micros: 5_000_000,
        ..Policy::default()
    };
    let mut client = QuicRecords::new(client.unwrap(), &cx, &[], &outgoing, policy).unwrap();
    let mut server = QuicRecords::new(server.unwrap(), &cx, &[], &incoming, policy).unwrap();
    let first = pointer(outgoing[0], 0);
    let latest = pointer(outgoing[0], 4);
    let audio = record(outgoing[1], 17);
    let video = record(outgoing[2], 23);
    client
        .send(
            &cx,
            Route::Datagram(outgoing[0]),
            &first,
            clock(&cx) + 2_000_000,
            || true,
        )
        .unwrap();
    // Install a held pointer before sending its successors. The pre-fix
    // implementation parks the second pointer and never reads either sibling.
    for _ in 0..64 {
        client.drive(&cx, Duration::ZERO, || true).await.unwrap();
        server
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        server
            .receive_ready(
                &cx,
                || true,
                |_| !through_readiness,
                |route, bytes| {
                    assert_eq!(route, Route::Datagram(incoming[0]));
                    assert_eq!(bytes, first);
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        if server.usage().remainder_bytes == first.len() {
            break;
        }
    }
    assert_eq!(server.usage().remainder_bytes, first.len());
    let mut original_receipt = None;
    server
        .receive_ready_with_receipts(
            &cx,
            || true,
            |_| true,
            |route, bytes, receipt| {
                assert_eq!(route, Route::Datagram(incoming[0]));
                assert_eq!(bytes, first);
                original_receipt = receipt;
                Ok(Disposition::Blocked)
            },
        )
        .unwrap();
    let original_receipt = original_receipt.expect("first pointer entered custody");
    // Distinguish the successor's custody from the original even if the first
    // two native polls happened in one timer microsecond.
    let next_micros = original_receipt
        .received_at_micros()
        .checked_add(1)
        .unwrap();
    sleep_until(Time::from_nanos(next_micros.checked_mul(1000).unwrap())).await;
    let successor_not_before = clock(&cx);
    assert!(successor_not_before > original_receipt.received_at_micros());
    for sequence in 1..=4 {
        client
            .send(
                &cx,
                Route::Datagram(outgoing[0]),
                &pointer(outgoing[0], sequence),
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
    }
    for (route, bytes) in [(outgoing[1], &audio), (outgoing[2], &video)] {
        client
            .send(
                &cx,
                Route::Datagram(route),
                bytes,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
    }

    let (mut audio_count, mut video_count) = (0, 0);
    let mut latest_seen = false;
    for _ in 0..64 {
        client.drive(&cx, Duration::ZERO, || true).await.unwrap();
        server
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        server
            .receive_ready(
                &cx,
                || true,
                |route| !through_readiness || route != Route::Datagram(incoming[0]),
                |route, bytes| {
                    if route == Route::Datagram(incoming[0]) {
                        assert!(!through_readiness);
                        return Ok(Disposition::Blocked);
                    }
                    if route == Route::Datagram(incoming[1]) {
                        assert_eq!(bytes, audio);
                        audio_count += 1;
                    } else {
                        assert_eq!(route, Route::Datagram(incoming[2]));
                        assert_eq!(bytes, video);
                        video_count += 1;
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(
            server.usage().remainder_bytes <= first.len(),
            "replaceable pointer created a second retained packet"
        );
        // A UDP sibling may overtake pointer motion. Observe the final
        // pointer as blocked custody before deciding the receive phase is done.
        server
            .receive_ready(
                &cx,
                || true,
                |route| route == Route::Datagram(incoming[0]),
                |route, bytes| {
                    assert_eq!(route, Route::Datagram(incoming[0]));
                    latest_seen |= bytes == latest;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        if audio_count == 1 && video_count == 1 && latest_seen {
            break;
        }
    }
    assert!(
        clock(&cx) < original_receipt.expires_at_micros(),
        "expiry must not conceal pointer head-of-line blocking"
    );
    assert_eq!((audio_count, video_count), (1, 1));
    assert!(latest_seen);
    assert!(!server.is_closed());
    assert_eq!(server.usage().remainder_bytes, latest.len());

    let mut latest_receipt = None;
    assert_eq!(
        server.receive_ready_with_receipts(
            &cx,
            || true,
            |_| true,
            |route, bytes, receipt| {
                assert_eq!(route, Route::Datagram(incoming[0]));
                assert_eq!(bytes, latest, "only the newest unsent position survives");
                let receipt = receipt.expect("replacement has its own original custody");
                assert!(receipt.received_at_micros() >= successor_not_before);
                assert_eq!(
                    receipt.expires_at_micros() - receipt.received_at_micros(),
                    policy.record_lifetime_micros
                );
                latest_receipt = Some(receipt);
                Ok(Disposition::Blocked)
            }
        ),
        Ok(0)
    );
    assert!(latest_receipt.is_some());
    assert_eq!(
        server.receive_ready_with_receipts(
            &cx,
            || true,
            |_| true,
            |route, bytes, receipt| {
                assert_eq!(route, Route::Datagram(incoming[0]));
                assert_eq!(bytes, latest);
                assert_eq!(receipt, latest_receipt, "delivery cannot renew custody");
                Ok(Disposition::Consumed)
            }
        ),
        Ok(1)
    );
    assert_eq!(server.usage().remainder_bytes, 0);
    assert_eq!(
        server.receive(&cx, || false, |_, _| panic!("revoked")),
        Err(Error::Unauthorized)
    );
    assert!(server.is_closed());
}

#[test]
fn blocked_pointer_readiness_does_not_starve_audio_or_video() {
    runtime().block_on(blocked_pointer_preserves_other_routes(true));
}

#[test]
fn blocked_pointer_handler_does_not_starve_audio_or_video() {
    runtime().block_on(blocked_pointer_preserves_other_routes(false));
}
