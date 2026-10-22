//! Real localhost UDP/TLS with fixture application authority. These cases
//! exercise bounded critical-stream accounting, not a live-tailnet benchmark.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{Disposition, Error, Policy, Route, SendAdmission, StreamRoute};
use fr_wire::{
    FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation, encode_progress,
};
use std::time::Duration;
use support::{clock, drive, pair, runtime};

fn packet(sequence: u64) -> Vec<u8> {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let mut out = vec![0; 1150];
    let len = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: sequence,
                reference: None,
                total_bytes: 4,
                stride: 4,
                capture_micros: sequence + 1,
            },
            observed_micros: sequence + 1,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut out,
    )
    .unwrap();
    out.truncate(len);
    out
}

#[test]
fn staggered_critical_records_keep_fifo_and_release_every_charge() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let policy = Policy {
            critical_send_records: 4,
            critical_send_bytes: 4096,
            ..Policy::default()
        };
        let mut p = pair(&cx, policy).await;
        let outgoing = Route::Stream(p.host_routes[0]);
        let incoming = Route::Stream(StreamRoute {
            outbound: false,
            ..p.host_routes[0]
        });
        let (mut sent, mut received) = (0_u64, 0_u64);
        let mut pending = None;
        for _ in 0..2000 {
            if sent < 64 {
                let (bytes, until) =
                    pending.get_or_insert_with(|| (packet(sent), clock(&cx) + 1_000_000));
                match p.server.send(&cx, outgoing, bytes, *until, || true) {
                    Ok(()) => {
                        sent += 1;
                        pending = None;
                    }
                    Err(Error::Backpressure) => {}
                    Err(error) => panic!("critical admission failed: {error:?}"),
                }
            }
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
            assert!(usage.critical_send_records <= policy.critical_send_records);
            assert!(usage.critical_send_bytes <= policy.critical_send_bytes);
            // These are total retained charges, including critical traffic.
            assert_eq!(usage.retained_send_records, usage.critical_send_records);
            assert_eq!(usage.retained_send_upper_bound, usage.critical_send_bytes);
            if sent == 64 && received == 64 && usage.critical_send_records == 0 {
                break;
            }
        }
        assert_eq!((sent, received), (64, 64));
        assert_eq!(p.server.usage().critical_send_records, 0);
        assert_eq!(p.server.usage().critical_send_bytes, 0);
        assert!(!p.server.is_closed());
    });
}

#[test]
fn joining_critical_records_does_not_release_budget_before_native_acknowledgment() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let policy = Policy {
            critical_send_records: 4,
            ..Policy::default()
        };
        let mut p = pair(&cx, policy).await;
        let route = Route::Stream(p.host_routes[0]);
        let until = clock(&cx) + 2_000_000;
        p.server
            .send(&cx, route, &packet(0), until, || true)
            .unwrap();
        // Start the flight, but do not service the peer or manufacture ACKs.
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        for sequence in 1..4 {
            p.server
                .send(&cx, route, &packet(sequence), until, || true)
                .unwrap();
        }
        assert_eq!(p.server.usage().critical_send_records, 4);
        assert_eq!(p.server.usage().critical_send_bytes, 4 * packet(0).len());
        assert_eq!(
            p.server.send(&cx, route, &packet(4), until, || true),
            Err(Error::Backpressure)
        );
        assert_eq!(p.server.usage().critical_send_records, 4);
        assert!(!p.server.is_closed());
        // Membership in a flight is not permission or a way around the gate.
        assert_eq!(p.server.tick(&cx, || false), Err(Error::Unauthorized));
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().critical_send_records, 0);
    });
}

#[test]
fn refused_critical_record_cannot_join_or_consume_the_existing_flight() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[0]);
        let until = clock(&cx) + 2_000_000;
        p.server
            .send(&cx, route, &packet(0), until, || true)
            .unwrap();
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert_eq!(
            p.server.send_prepared(
                &cx,
                route,
                &packet(1),
                until,
                || true,
                || Err::<(), _>("not ready"),
            ),
            Ok(SendAdmission::Refused("not ready"))
        );
        assert_eq!(p.server.usage().critical_send_records, 1);
        assert_eq!(p.server.usage().critical_send_bytes, packet(0).len());
        assert!(!p.server.is_closed());
        let mut checks = 0;
        assert_eq!(
            p.server.send(&cx, route, &packet(1), until, || {
                checks += 1;
                checks == 1
            }),
            Err(Error::Unauthorized)
        );
        assert_eq!(checks, 2);
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().critical_send_records, 0);
    });
}
