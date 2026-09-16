//! Bounded reliable bulk turns over real authenticated UDP/TLS connections.
//! Authority is an explicit fixture; no live-tailnet qualification is claimed.
#![cfg(target_os = "linux")]
mod support;
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::*;
use fr_wire::{MediaLimits, RecoveryChunk, encode_recovery};
use std::time::Duration;
use support::{clock, drive, pair, runtime};

fn recovery_packet(binding: u32, count: usize) -> Vec<u8> {
    let mut out = vec![0; 65536];
    let data = vec![81; count];
    let l = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let size = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: u32::try_from(count).unwrap(),
            offset: 0,
            capture_micros: 17,
            bytes: &data,
        },
        binding,
        &l,
        &mut out,
    )
    .unwrap();
    out.truncate(size);
    out
}
#[test]
fn reliable_turn_advances_a_bounded_burst_without_an_idle_wait_per_prefix() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for (payload, completes) in [(6_500, true), (8_000, false)] {
            let mut p = pair(&cx, Policy::default()).await;
            assert!(p.server.has_route(Route::Datagram(p.video)));
            let packet = recovery_packet(2, payload);
            p.server
                .send(
                    &cx,
                    Route::Stream(p.host_routes[1]),
                    &packet,
                    clock(&cx) + 2_000_000,
                    || true,
                )
                .unwrap();
            // Only ONE sender turn. A <=7,200-byte record can use available
            // native credit now; a larger record still needs another turn.
            p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
            let mut received = 0;
            for _ in 0..8 {
                p.client
                    .drive(&cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                p.client
                    .receive(
                        &cx,
                        || true,
                        |_, bytes| {
                            assert_eq!(bytes, packet);
                            received += 1;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
            assert_eq!(received, usize::from(completes));
            if !completes {
                for _ in 0..30 {
                    drive(&cx, &mut p).await;
                    p.client
                        .receive(
                            &cx,
                            || true,
                            |_, bytes| {
                                assert_eq!(bytes, packet);
                                received += 1;
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                    if received == 1 {
                        break;
                    }
                }
                assert_eq!(received, 1);
            }
        }
    });
}

#[test]
fn a_send_burst_keeps_checking_authority_between_native_prefixes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let packet = recovery_packet(2, 32_000);
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &packet,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let mut checks = 0;
        let result = p
            .server
            .drive(&cx, Duration::ZERO, || {
                checks += 1;
                checks < 8
            })
            .await;
        assert_eq!(result, Err(Error::Unauthorized));
        assert_eq!(checks, 8);
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().retained_send_records, 0);
        for _ in 0..8 {
            p.client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            assert_eq!(
                p.client
                    .receive(
                        &cx,
                        || true,
                        |_, _| {
                            panic!("a revoked partial burst must not become a complete record")
                        }
                    )
                    .unwrap(),
                0
            );
        }
    });
}
