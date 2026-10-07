//! Actual TLS/UDP admission tests. Record validity and authority are synthetic;
//! these tests are not native-media, impaired-network or tailnet qualification.
#![cfg(target_os = "linux")]
mod support;

use asupersync::{cx::Cx, types::CancelKind};
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{Disposition, Error, Policy, Route, SendAdmission};
use fr_wire::{Fragment, FrameDescriptor, MediaLimits, RecoveryChunk};
use std::{cell::Cell, time::Duration};
use support::{clock, drive, pair, runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    ExpiredReference,
}

fn recovery_packet(value: u8) -> Vec<u8> {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let mut bytes = vec![0; 65536];
    let payload = [value; 100];
    let len = fr_wire::encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 100,
            offset: 0,
            capture_micros: 17,
            bytes: &payload,
        },
        2,
        &limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(len);
    bytes
}

#[test]
fn final_record_refusal_preserves_admitted_records_and_the_original_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        let first = recovery_packet(41);
        let rejected = recovery_packet(97);
        let next = recovery_packet(42);
        let until = clock(&cx) + 2_000_000;
        p.server.send(&cx, route, &first, until, || true).unwrap();
        let usage = p.server.usage();
        let mut checks = 0;
        assert_eq!(
            p.server.send_prepared(
                &cx,
                route,
                &rejected,
                until,
                || true,
                || {
                    checks += 1;
                    if checks == 2 {
                        Err(Refusal::ExpiredReference)
                    } else {
                        Ok(())
                    }
                },
            ),
            Ok(SendAdmission::Refused(Refusal::ExpiredReference))
        );
        assert_eq!(checks, 2, "must reach the post-allocation record gate");
        assert!(!p.server.is_closed());
        assert_eq!(p.server.usage(), usage, "refusal must not admit queue ownership");
        p.server.send(&cx, route, &next, until, || true).unwrap();
        let mut received = Vec::new();
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        received.push(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received.len() == 2 && p.server.usage().retained_send_records == 0 {
                break;
            }
        }
        assert_eq!(received, [first, next]);
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn final_datagram_refusal_never_enters_the_native_datagram_queue() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
        let mut bytes = [0; 1150];
        let payload = [7; 100];
        let len = fr_wire::encode_fragment(
            Fragment {
                descriptor: FrameDescriptor {
                    frame: 1,
                    reference: None,
                    total_bytes: 100,
                    stride: limits.fragment_stride(),
                    capture_micros: 17,
                },
                index: 0,
                bytes: &payload,
            },
            p.video.binding,
            &limits,
            &mut bytes,
        )
        .unwrap();
        let mut checks = 0;
        assert_eq!(
            p.server.send_prepared(
                &cx,
                Route::Datagram(p.video),
                &bytes[..len],
                clock(&cx) + 2_000_000,
                || true,
                || {
                    checks += 1;
                    if checks == 2 {
                        Err(Refusal::ExpiredReference)
                    } else {
                        Ok(())
                    }
                },
            ),
            Ok(SendAdmission::Refused(Refusal::ExpiredReference))
        );
        assert_eq!(checks, 2);
        assert!(!p.server.is_closed());
        let sentinel = recovery_packet(42);
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &sentinel,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let mut received = 0;
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |route, bytes| {
                        assert!(matches!(route, Route::Stream(_)), "rejected datagram escaped");
                        assert_eq!(bytes, sentinel);
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received == 1 && p.server.usage().retained_send_records == 0 {
                break;
            }
        }
        assert_eq!(received, 1);
    });
}

#[test]
fn source_revocation_during_the_final_record_check_overrides_a_typed_refusal() {
    for refuse in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx, Policy::default()).await;
            let live = Cell::new(true);
            let mut checks = 0;
            assert_eq!(
                p.server.send_prepared(
                    &cx,
                    Route::Stream(p.host_routes[1]),
                    &recovery_packet(41),
                    clock(&cx) + 2_000_000,
                    || live.get(),
                    || {
                        checks += 1;
                        if checks == 2 {
                            live.set(false);
                            if refuse {
                                return Err(Refusal::ExpiredReference);
                            }
                        }
                        Ok(())
                    },
                ),
                Err(Error::Unauthorized)
            );
            assert_eq!(checks, 2);
            assert!(p.server.is_closed());
            assert_eq!(p.server.usage().retained_send_records, 0);
        });
    }
}

#[test]
fn cancellation_in_a_record_callback_is_terminal_not_recoverable_media() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let mut checks = 0;
        assert_eq!(
            p.server.send_prepared(
                &cx,
                Route::Stream(p.host_routes[1]),
                &recovery_packet(41),
                clock(&cx) + 2_000_000,
                || true,
                || {
                    checks += 1;
                    if checks == 2 {
                        cx.cancel_fast(CancelKind::User);
                        Err(Refusal::ExpiredReference)
                    } else {
                        Ok(())
                    }
                },
            ),
            Err(Error::Cancelled)
        );
        assert_eq!(checks, 2);
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn expired_retained_record_cannot_be_hidden_by_an_application_refusal() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        p.server
            .send(&cx, route, &recovery_packet(41), clock(&cx) + 2_000_000, || true)
            .unwrap();
        // Do not drive or acknowledge this admitted record. The production
        // delivery allowance is bounded above by two seconds, not refreshed.
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        assert_eq!(
            p.server.send_prepared::<Refusal>(
                &cx,
                route,
                &recovery_packet(42),
                clock(&cx) + 2_000_000,
                || true,
                || panic!("expired retained record reached application admission"),
            ),
            Err(Error::Expired)
        );
        assert!(p.server.is_closed());
    });
}

#[test]
fn initial_record_refusal_admits_nothing_and_does_not_call_the_final_gate() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let mut checks = 0;
        assert_eq!(
            p.server.send_prepared(
                &cx,
                Route::Stream(p.host_routes[1]),
                &recovery_packet(41),
                clock(&cx) + 2_000_000,
                || true,
                || {
                    checks += 1;
                    Err(Refusal::ExpiredReference)
                },
            ),
            Ok(SendAdmission::Refused(Refusal::ExpiredReference))
        );
        assert_eq!(checks, 1);
        assert!(!p.server.is_closed());
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn malformed_records_cannot_hide_behind_a_typed_refusal() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        let before = p.server.usage();
        let mut packet = recovery_packet(41);
        packet[0] = 0;
        assert_eq!(
            p.server.send_prepared::<Refusal>(
                &cx,
                route,
                &packet,
                clock(&cx) + 2_000_000,
                || true,
                || panic!("malformed record reached the application gate"),
            ),
            Err(Error::WrongRoute)
        );
        assert_eq!(p.server.usage(), before);
        assert!(!p.server.is_closed());
    });
}
