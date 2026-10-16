//! Real UDP/TLS regression coverage for opportunistic bulk send batches.
//! Application authority is a fixture; this is not impaired-tailnet evidence.
#![cfg(target_os = "linux")]
#[allow(dead_code)] // The shared TLS fixture also exposes helpers for other tests.
mod support;

use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{Disposition, Error, Policy, Route, StreamRoute};
use fr_wire::{MediaLimits, RecoveryChunk, encode_recovery};
use std::time::Duration;
use support::{clock, drive, pair, runtime};

fn packet(sequence: u64) -> Vec<u8> {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let data = [u8::try_from(sequence).unwrap(); 1024];
    let mut out = vec![0; 4096];
    let len = encode_recovery(
        RecoveryChunk {
            frame: sequence,
            total_bytes: 1024,
            offset: 0,
            capture_micros: sequence,
            bytes: &data,
        },
        2,
        &limits,
        &mut out,
    )
    .unwrap();
    out.truncate(len);
    out
}

#[test]
fn staggered_bulk_batches_keep_fifo_bounds_and_release_every_charge() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let policy = Policy {
            retained_send_records: 4,
            retained_send_bytes: 8192,
            ..Policy::default()
        };
        let mut p = pair(&cx, policy).await;
        let outgoing = Route::Stream(p.host_routes[1]);
        let incoming = Route::Stream(StreamRoute {
            outbound: false,
            ..p.host_routes[1]
        });
        let (mut sent, mut received) = (0_u64, 0_u64);
        let mut pending = None;
        for _ in 0..2000 {
            if sent < 128 {
                let (bytes, until) =
                    pending.get_or_insert_with(|| (packet(sent), clock(&cx) + 1_000_000));
                match p.server.send(&cx, outgoing, bytes, *until, || true) {
                    Ok(()) => {
                        sent += 1;
                        pending = None;
                    }
                    Err(Error::Backpressure) => {}
                    Err(error) => panic!("bulk admission failed: {error:?}"),
                }
            }
            // One new record per scheduler turn, not a prequeued batch. Retry
            // the same bytes/deadline if the original global budget is full.
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |route, bytes| {
                        assert_eq!(route, incoming);
                        assert_eq!(bytes, packet(received));
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            let usage = p.server.usage();
            assert!(usage.retained_send_records <= policy.retained_send_records);
            assert!(usage.retained_send_upper_bound <= policy.retained_send_bytes);
            assert_eq!(usage.critical_send_records, 0);
            // Released credit proves the host's epochs were ACKed, not that
            // this turn's receive already delivered the final record.
            if sent == 128 && received == 128 && usage.retained_send_records == 0 {
                break;
            }
        }
        assert_eq!((sent, received), (128, 128));
        assert_eq!(p.server.usage().retained_send_records, 0);
        assert_eq!(p.server.usage().retained_send_upper_bound, 0);
        assert!(!p.server.is_closed());
    });
}

#[test]
fn joining_or_waiting_bulk_never_extends_the_earliest_delivery_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for later_expires_first in [false, true] {
            let mut p = pair(&cx, Policy::default()).await;
            let route = Route::Stream(p.host_routes[1]);
            let warm_until = clock(&cx) + 1_000_000;
            p.server
                .send(&cx, route, &packet(0), warm_until, || true)
                .unwrap();
            while p.server.usage().retained_send_records != 0 {
                assert!(clock(&cx) < warm_until);
                drive(&cx, &mut p).await;
                p.client
                    .receive(&cx, || true, |_, _| Ok(Disposition::Consumed))
                    .unwrap();
            }
            let now = clock(&cx);
            let (first, later) = if later_expires_first {
                (1_900_000, 40_000)
            } else {
                (40_000, 1_900_000)
            };
            p.server
                .send(&cx, route, &packet(1), now + first, || true)
                .unwrap();
            // No further client ACK service. Exercise an open or already
            // closed bulk batch without relying on sub-10ms wall-clock timing.
            p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
            p.server
                .send(&cx, route, &packet(2), now + later, || true)
                .unwrap();
            assert_eq!(p.server.usage().retained_send_records, 2);
            asupersync::time::sleep(cx.now(), Duration::from_millis(45)).await;
            assert_eq!(p.server.tick(&cx, || true), Ok(()));
            let mut result = Ok(());
            while result.is_ok() {
                assert!(
                    clock(&cx) < now + 1_900_000,
                    "later admission extended a deadline"
                );
                asupersync::time::sleep(cx.now(), Duration::from_millis(10)).await;
                result = p.server.tick(&cx, || true);
            }
            assert_eq!(result, Err(Error::Expired));
            assert!(p.server.is_closed());
            assert_eq!(p.server.usage().retained_send_records, 0);
        }
    });
}
