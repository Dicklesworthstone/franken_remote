//! Bounded reliable bulk turns over real authenticated UDP/TLS connections.
//! Authority is an explicit fixture; no live-tailnet qualification is claimed.
#![cfg(target_os = "linux")]
mod support;
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::*;
use fr_wire::{
    FrameDescriptor, MediaLimits, PipelineState, Progress, RecoveryChunk, SourceObservation,
    encode_progress, encode_recovery,
};
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

macro_rules! run_test {
    ($cx:ident, $body:expr) => {{
        runtime().block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
}

#[test]
fn reliable_turn_advances_a_bounded_burst_without_an_idle_wait_per_prefix() {
    run_test!(cx, {
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
    run_test!(cx, {
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
                        },
                    )
                    .unwrap(),
                0
            );
        }
    });
}

fn progress_packet(binding: u32) -> Vec<u8> {
    let mut out = vec![0; 1150];
    let l = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let size = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 1,
                total_bytes: 100,
                stride: 100,
                capture_micros: 42,
                reference: Some(0),
            },
            observed_micros: 42,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
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
#[allow(clippy::too_many_lines)]
fn input_latency_stays_bounded_while_bulk_is_saturated_with_logged_queue_depths() {
    run_test!(cx, {
        // Configure policy with tight bulk limits and separate critical reservation
        let policy = Policy {
            retained_send_records: 2,
            retained_send_bytes: 4000,
            critical_send_records: 4,
            critical_send_bytes: 2000,
            critical_connection_credit: 4096,
            ..Policy::default()
        };
        let mut p = pair(&cx, policy).await;

        let bulk_route = Route::Stream(p.host_routes[1]); // Priority::Bulk, binding 2
        let critical_route = Route::Stream(p.host_routes[0]); // Priority::Critical, binding 3

        let u0 = p.server.usage();
        eprintln!(
            "[QueueDepth Init] bulk_records={}/{} bulk_bytes={}/{} critical_records={}/{} critical_bytes={}/{}",
            u0.retained_send_records,
            policy.retained_send_records,
            u0.retained_send_upper_bound,
            policy.retained_send_bytes,
            u0.critical_send_records,
            policy.critical_send_records,
            u0.critical_send_bytes,
            policy.critical_send_bytes
        );
        assert_eq!(u0.retained_send_records, 0);
        assert_eq!(u0.critical_send_records, 0);

        // Saturate the bulk channel: send 2 records of 1800 bytes each (total 3600 <= 4000)
        let bulk_p1 = recovery_packet(2, 1800);
        let bulk_p2 = recovery_packet(2, 1800);
        let bulk_p3 = recovery_packet(2, 1800);

        p.server
            .send(&cx, bulk_route, &bulk_p1, clock(&cx) + 2_000_000, || true)
            .unwrap();
        p.server
            .send(&cx, bulk_route, &bulk_p2, clock(&cx) + 2_000_000, || true)
            .unwrap();

        // Third bulk record MUST be rejected with Backpressure because records = 2/2
        let err = p
            .server
            .send(&cx, bulk_route, &bulk_p3, clock(&cx) + 2_000_000, || true);
        assert_eq!(err, Err(Error::Backpressure));

        let u_sat = p.server.usage();
        eprintln!(
            "[QueueDepth BulkSaturated] bulk_records={}/{} bulk_bytes={}/{} critical_records={}/{} critical_bytes={}/{}",
            u_sat.retained_send_records,
            policy.retained_send_records,
            u_sat.retained_send_upper_bound,
            policy.retained_send_bytes,
            u_sat.critical_send_records,
            policy.critical_send_records,
            u_sat.critical_send_bytes,
            policy.critical_send_bytes
        );
        assert_eq!(u_sat.retained_send_records, policy.retained_send_records);

        // While bulk is completely saturated and under backpressure, send critical messages
        let crit_p1 = progress_packet(3);
        let crit_p2 = progress_packet(3);

        // Critical sends MUST succeed despite bulk queue being saturated!
        p.server
            .send(
                &cx,
                critical_route,
                &crit_p1,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        p.server
            .send(
                &cx,
                critical_route,
                &crit_p2,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();

        let u_crit = p.server.usage();
        eprintln!(
            "[QueueDepth CriticalInjected] bulk_records={}/{} bulk_bytes={}/{} critical_records={}/{} critical_bytes={}/{}",
            u_crit.retained_send_records,
            policy.retained_send_records,
            u_crit.retained_send_upper_bound,
            policy.retained_send_bytes,
            u_crit.critical_send_records,
            policy.critical_send_records,
            u_crit.critical_send_bytes,
            policy.critical_send_bytes
        );
        assert_eq!(u_crit.retained_send_records, 4);
        assert_eq!(u_crit.critical_send_records, 2);

        // Drive the connection. Critical messages MUST be delivered promptly without
        // being delayed behind all bulk bytes.
        let mut critical_received = 0;
        let mut bulk_received = 0;

        let start_time = clock(&cx);
        for turn in 0..50 {
            drive(&cx, &mut p).await;

            let u_step = p.server.usage();
            eprintln!(
                "[QueueDepth Turn {}] bulk_records={} bulk_bytes={} critical_records={} critical_bytes={}",
                turn,
                u_step.retained_send_records,
                u_step.retained_send_upper_bound,
                u_step.critical_send_records,
                u_step.critical_send_bytes
            );

            p.client
                .receive(
                    &cx,
                    || true,
                    |route, _bytes| {
                        if route
                            == Route::Stream(StreamRoute {
                                outbound: false,
                                ..p.host_routes[0]
                            })
                        {
                            critical_received += 1;
                        } else if route
                            == Route::Stream(StreamRoute {
                                outbound: false,
                                ..p.host_routes[1]
                            })
                        {
                            bulk_received += 1;
                        }
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();

            if critical_received == 2 {
                let latency = clock(&cx) - start_time;
                eprintln!(
                    "[CriticalTraffic Delivered] critical_latency_micros={latency} turns={turn} bulk_received_so_far={bulk_received}"
                );
                // Critical traffic arrived within bounded turns
                assert!(turn <= 15, "critical traffic took too many turns ({turn})");
                break;
            }
        }

        assert_eq!(
            critical_received, 2,
            "both critical packets must be received"
        );

        // Now drain remaining bulk traffic
        for _ in 0..100 {
            if bulk_received == 2 {
                break;
            }
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |route, _bytes| {
                        if route
                            == Route::Stream(StreamRoute {
                                outbound: false,
                                ..p.host_routes[1]
                            })
                        {
                            bulk_received += 1;
                        }
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }

        assert_eq!(bulk_received, 2, "all bulk packets received");
        let u_final = p.server.usage();
        eprintln!(
            "[QueueDepth Drained] bulk_records={} critical_records={}",
            u_final.retained_send_records, u_final.critical_send_records
        );
        assert_eq!(u_final.retained_send_records, 0);
        assert_eq!(u_final.critical_send_records, 0);
    });
}

#[path = "native_bulk/epochs.rs"]
mod epochs;
