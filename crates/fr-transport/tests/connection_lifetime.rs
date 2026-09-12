//! Real native UDP/TLS with an independently revocable, connection-bound guard.
#![cfg(target_os = "linux")]
mod support;
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::*;
use fr_wire::{MediaLimits, RecoveryChunk, encode_recovery};
use std::{future::Future, pin::pin, task::Poll, time::Duration};
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
fn connection_lifetime_is_immutable_and_a_per_call_true_cannot_override_it() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_lifetime_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        assert_eq!(
            p.server.retain_lifetime_check(&cx, Arc::new(|| true)),
            Err(Error::InvalidPolicy)
        );
        live.store(false, Ordering::Release);
        assert_eq!(
            p.server.send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &recovery_packet(2, 8),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
    });
}
#[test]
fn pending_native_io_rechecks_connection_lifetime_before_resuming() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_lifetime_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        let mut tested = false;
        for _ in 0..16 {
            let mut io = pin!(p.server.drive(&cx, Duration::from_millis(100), || true));
            let pending =
                std::future::poll_fn(|task| Poll::Ready(io.as_mut().poll(task).is_pending())).await;
            if pending {
                live.store(false, Ordering::Release);
                let result = std::future::poll_fn(|task| Poll::Ready(io.as_mut().poll(task))).await;
                assert_eq!(result, Poll::Ready(Err(Error::Unauthorized)));
                tested = true;
                break;
            }
        }
        assert!(
            tested,
            "must exercise a genuinely pending native I/O future"
        );
        assert!(p.server.is_closed());
    });
}
#[test]
fn connection_lifetime_checks_receive_dispatch_and_refuses_failed_installation() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.client
            .retain_lifetime_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &recovery_packet(2, 8),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        for _ in 0..4 {
            drive(&cx, &mut p).await;
        }
        live.store(false, Ordering::Release);
        assert_eq!(
            p.client.receive(
                &cx,
                || true,
                |_, _| panic!("expired lifetime cannot dispatch")
            ),
            Err(Error::Unauthorized)
        );
        assert!(p.client.is_closed());
        assert_eq!(
            p.server.retain_lifetime_check(&cx, Arc::new(|| false)),
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
    });
}

#[test]
fn datagram_admission_cannot_override_a_revoked_connection_lifetime() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_lifetime_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
        let mut bytes = [0; 1150];
        let n = fr_wire::encode_fragment(
            fr_wire::Fragment {
                descriptor: fr_wire::FrameDescriptor {
                    frame: 1,
                    reference: Some(0),
                    capture_micros: 17,
                    total_bytes: 4,
                    stride: 4,
                },
                index: 0,
                bytes: b"data",
            },
            p.video.binding,
            &limits,
            &mut bytes,
        )
        .unwrap();
        live.store(false, Ordering::Release);
        assert_eq!(
            p.server.send(
                &cx,
                Route::Datagram(p.video),
                &bytes[..n],
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
    });
}
