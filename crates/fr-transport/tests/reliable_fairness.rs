//! Prefix fairness over real authenticated UDP/TLS. Payloads exercise FRD0
//! framing only; these tests do not qualify media, a tailnet, or WAN performance.
#![cfg(target_os = "linux")]
#[allow(dead_code)] // The shared TLS fixture also exposes helpers for other tests.
mod support;

use asupersync::cx::Cx;
use fr_transport::quic::{
    ALPN, Disposition, Messages, Policy, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::HEADER_BYTES;
use std::time::Duration;
use support::{clock, native_pair, runtime};

/// A sender turn that also services the peer's ACKs. `drive(Duration::ZERO)`
/// stages prefixes but does not process received packets, so no record's
/// charge can be released in a loop of zero-wait turns.
const ACK_TURN: Duration = Duration::from_millis(1);

struct Peers {
    server: QuicRecords,
    client: QuicRecords,
    outgoing: [StreamRoute; 3],
}

async fn peers(cx: &Cx) -> Peers {
    let (client, server) = native_pair(cx, "localhost", ALPN).await;
    let mut server = server.unwrap();
    let outgoing = std::array::from_fn(|index| StreamRoute {
        stream: server.connection_mut().open_uni_stream(cx).unwrap(),
        binding: u32::try_from(index + 1).unwrap(),
        messages: Messages::Exact(0x0032),
        priority: if index == 2 {
            Priority::Critical
        } else {
            Priority::Bulk
        },
        outbound: true,
        maximum: if index == 2 { 1150 } else { 65536 },
    });
    let incoming = outgoing.map(|route| StreamRoute {
        outbound: false,
        ..route
    });
    Peers {
        server: QuicRecords::new(server, cx, &outgoing, &[], Policy::default()).unwrap(),
        client: QuicRecords::new(client.unwrap(), cx, &incoming, &[], Policy::default()).unwrap(),
        outgoing,
    }
}

fn record(route: StreamRoute, marker: u8, length: usize) -> Vec<u8> {
    assert!(length >= HEADER_BYTES);
    let mut bytes = vec![marker; length];
    bytes[..HEADER_BYTES].fill(0);
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[6..8].copy_from_slice(&0x0032_u16.to_be_bytes());
    bytes[12..16].copy_from_slice(&u32::try_from(length - HEADER_BYTES).unwrap().to_be_bytes());
    bytes[16..20].copy_from_slice(&route.binding.to_be_bytes());
    bytes
}

fn send(cx: &Cx, peers: &mut Peers, route: usize, record: &[u8]) {
    peers
        .server
        .send(
            cx,
            Route::Stream(peers.outgoing[route]),
            record,
            clock(cx) + 2_000_000,
            || true,
        )
        .unwrap();
}

fn receive(cx: &Cx, peers: &mut Peers, received: &mut Vec<(u32, Vec<u8>)>) {
    peers
        .client
        .receive(
            cx,
            || true,
            |route, bytes| {
                let Route::Stream(route) = route else {
                    panic!("no datagram route installed")
                };
                received.push((route.binding, bytes.to_vec()));
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
}

async fn receive_queued(cx: &Cx, peers: &mut Peers, received: &mut Vec<(u32, Vec<u8>)>) {
    // Drive ONLY the receiver. A sender turn has exactly eight bulk-prefix
    // slots, so a missing small record cannot be rescued by staging the rest
    // of the large record during this observation window.
    for _ in 0..16 {
        peers
            .client
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        receive(cx, peers, received);
    }
}

#[test]
fn a_large_bulk_record_cannot_hide_another_lanes_small_record() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut peers = peers(&cx).await;
        let large = record(peers.outgoing[0], 11, 32_000);
        let following = record(peers.outgoing[0], 12, 100);
        let small = record(peers.outgoing[1], 23, 100);
        send(&cx, &mut peers, 0, &large);
        send(&cx, &mut peers, 0, &following);
        send(&cx, &mut peers, 1, &small);

        peers
            .server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        let mut received = Vec::new();
        receive_queued(&cx, &mut peers, &mut received).await;
        assert_eq!(received, [(2, small.clone())]);
        // ACKs have not yet been driven on the sender. Neither successful
        // staging nor client consumption releases retained-send accounting.
        assert_eq!(peers.server.usage().retained_send_records, 3);
        assert_eq!(
            peers.server.usage().retained_send_upper_bound,
            large.len() + following.len() + small.len()
        );

        for _ in 0..128 {
            peers.server.drive(&cx, ACK_TURN, || true).await.unwrap();
            peers
                .client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            receive(&cx, &mut peers, &mut received);
            if received.len() == 3 {
                break;
            }
        }
        // Cross-stream interleaving does not permit the short record queued
        // behind the large record ON ITS OWN stream to overtake that record.
        assert_eq!(received, [(2, small), (1, large), (1, following)]);
        for _ in 0..128 {
            if peers.server.usage().retained_send_records == 0 {
                break;
            }
            peers.server.drive(&cx, ACK_TURN, || true).await.unwrap();
            peers
                .client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
        }
        assert_eq!(peers.server.usage().retained_send_records, 0);
        assert_eq!(peers.server.usage().retained_send_upper_bound, 0);
    });
}

#[test]
fn critical_records_still_preempt_both_bulk_lanes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut peers = peers(&cx).await;
        let large = record(peers.outgoing[0], 11, 32_000);
        let small = record(peers.outgoing[1], 23, 100);
        let critical = record(peers.outgoing[2], 37, 100);
        send(&cx, &mut peers, 0, &large);
        send(&cx, &mut peers, 1, &small);
        send(&cx, &mut peers, 2, &critical);

        // A critical prefix gets the first slot. The following bounded bulk
        // opportunity can only begin the large record, not complete it or
        // overtake its stream with the later small record.
        peers
            .server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        let mut received = Vec::new();
        receive_queued(&cx, &mut peers, &mut received).await;
        assert_eq!(received, [(3, critical)]);
        assert!(!peers.server.is_closed());
    });
}

#[test]
fn a_continuous_critical_backlog_cannot_starve_reliable_recovery() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut peers = peers(&cx).await;
        let recovery = record(peers.outgoing[0], 19, 3600);
        send(&cx, &mut peers, 0, &recovery);
        let critical: Vec<_> = (0..8)
            .map(|sequence| record(peers.outgoing[2], 40 + sequence, 100))
            .collect();
        for bytes in &critical {
            send(&cx, &mut peers, 2, bytes);
        }
        let mut received = Vec::new();
        for turn in 1..=4 {
            peers
                .server
                .drive(&cx, Duration::ZERO, || true)
                .await
                .unwrap();
            // Critical work remains queued at every turn, not merely waiting
            // for an ACK. Strict priority used to prevent ANY recovery prefix.
            assert!(!peers.server.send_staged(peers.outgoing[2]).unwrap());
            receive_queued(&cx, &mut peers, &mut received).await;
            let critical_received: Vec<_> = received
                .iter()
                .filter(|(binding, _)| *binding == 3)
                .map(|(_, bytes)| bytes.clone())
                .collect();
            assert_eq!(critical_received, critical[..turn]);
            let recovery_received: Vec<_> = received
                .iter()
                .filter(|(binding, _)| *binding == 1)
                .map(|(_, bytes)| bytes.clone())
                .collect();
            if turn < 4 {
                // Only one 900-byte bulk prefix per critical turn: recovery
                // cannot spend the ordinary eight-prefix burst here.
                assert_eq!(recovery_received.len(), 0);
            } else {
                assert_eq!(recovery_received, std::slice::from_ref(&recovery));
            }
        }
        // No sender receive turn ran: native staging and application receipt
        // are not ACK evidence and cannot reclaim either class's quota.
        assert_eq!(peers.server.usage().retained_send_records, 9);
        assert_eq!(
            peers.server.usage().retained_send_upper_bound,
            recovery.len() + critical.iter().map(Vec::len).sum::<usize>()
        );
    });
}

#[test]
fn the_bulk_opportunity_after_critical_work_still_rechecks_authority() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut peers = peers(&cx).await;
        let recovery = record(peers.outgoing[0], 19, 100);
        let critical = record(peers.outgoing[2], 40, 100);
        send(&cx, &mut peers, 0, &recovery);
        send(&cx, &mut peers, 2, &critical);
        let mut checks = 0;
        let result = peers
            .server
            .drive(&cx, Duration::ZERO, || {
                checks += 1;
                checks < 4
            })
            .await;
        assert_eq!(result, Err(fr_transport::quic::Error::Unauthorized));
        assert!(peers.server.is_closed());
        assert_eq!(peers.server.usage().retained_send_records, 0);
        let mut received = Vec::new();
        receive_queued(&cx, &mut peers, &mut received).await;
        // Even if critical bytes already reached the socket, revocation must
        // fence the following bulk prefix; existing effects are not rolled back.
        assert!(
            received
                .iter()
                .all(|(binding, bytes)| *binding == 3 && bytes == &critical)
        );
    });
}

#[test]
fn a_record_behind_an_unacknowledged_one_stages_at_once_and_stays_charged() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut peers = peers(&cx).await;
        let first = record(peers.outgoing[0], 11, 100);
        let following = record(peers.outgoing[0], 12, 100);
        let other = record(peers.outgoing[1], 23, 100);
        send(&cx, &mut peers, 0, &first);
        peers
            .server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        assert!(peers.server.send_staged(peers.outgoing[0]).unwrap());
        assert_eq!(peers.server.usage().retained_send_records, 1);

        // The peer has not driven I/O since admission, so `first` has no ACK.
        // Until fr-00nc a record admitted more than 10 ms later waited for the
        // whole previous batch to be acknowledged: one more RTT per record.
        std::thread::sleep(Duration::from_millis(11));
        send(&cx, &mut peers, 0, &following);
        send(&cx, &mut peers, 1, &other);
        peers
            .server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        assert!(peers.server.send_staged(peers.outgoing[0]).unwrap());
        assert!(peers.server.send_staged(peers.outgoing[1]).unwrap());
        // Staging releases nothing: all three wait for the peer's ACKs.
        assert_eq!(peers.server.usage().retained_send_records, 3);

        let mut received = Vec::new();
        receive_queued(&cx, &mut peers, &mut received).await;
        assert_eq!(received.len(), 3);
        let on_first_lane: Vec<_> = received
            .iter()
            .filter(|(binding, _)| *binding == 1)
            .map(|(_, bytes)| bytes.clone())
            .collect();
        assert_eq!(on_first_lane, [first, following], "stream order");
        assert!(received.contains(&(2, other)));
        // The receiver acknowledged, but the sender has not processed it yet.
        assert_eq!(peers.server.usage().retained_send_records, 3);

        for _ in 0..128 {
            peers.server.drive(&cx, ACK_TURN, || true).await.unwrap();
            peers
                .client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            if peers.server.usage().retained_send_records == 0 {
                break;
            }
        }
        assert_eq!(peers.server.usage().retained_send_records, 0);
        assert_eq!(peers.server.usage().retained_send_upper_bound, 0);
    });
}
