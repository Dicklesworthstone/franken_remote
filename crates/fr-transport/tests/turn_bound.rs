//! A drive turn's 250 ms bound over real authenticated UDP/TLS: time the
//! thread spends preempted between two polls of a turn does not fail a turn
//! that can finish. On a loaded macOS host one 252 ms poll of frd's hub made
//! the next poll of a turn with a 10 ms receive wait fail as Expired, closing
//! the session. Payloads are framing fixtures; no codec or tailnet claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)] // The shared TLS fixture also exposes helpers for other tests.
mod support;

use asupersync::cx::Cx;
use fr_transport::quic::{Disposition, Policy, Route, StreamRoute};
use fr_wire::{MediaLimits, RecoveryChunk, encode_recovery};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};
use support::{clock, drive, pair, runtime};

fn recovery_packet(count: usize) -> Vec<u8> {
    let mut out = vec![0; 65536];
    let data = vec![81; count];
    let limits =
        MediaLimits::new(fr_core::limits::ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let size = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: u32::try_from(count).unwrap(),
            offset: 0,
            capture_micros: 17,
            bytes: &data,
        },
        2,
        &limits,
        &mut out,
    )
    .unwrap();
    out.truncate(size);
    out
}

#[test]
fn a_turn_whose_thread_was_preempted_past_its_bound_still_finishes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let mut parked = false;
        for _ in 0..20 {
            let mut turn = pin!(p.server.drive(&cx, Duration::from_millis(10), || true));
            match poll_fn(|task| Poll::Ready(turn.as_mut().poll(task))).await {
                // Peer traffic finished this turn at once: park the next one.
                Poll::Ready(result) => result.unwrap(),
                Poll::Pending => {
                    parked = true;
                    // The turn waits on its 10 ms receive; the thread is held
                    // outside it, as a preempted hub poll held frd's.
                    std::thread::sleep(Duration::from_millis(300));
                    assert_eq!(turn.await, Ok(()), "a preempted turn failed");
                    break;
                }
            }
        }
        assert!(parked, "a quiet connection must reach a pending wait");
        assert!(!p.server.is_closed());

        // The connection still carries records afterwards.
        let route = Route::Stream(p.host_routes[1]);
        let packet = recovery_packet(600);
        p.server
            .send(&cx, route, &packet, clock(&cx) + 2_000_000, || true)
            .unwrap();
        let mut received = vec![];
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |r, b| {
                        assert_eq!(
                            r,
                            Route::Stream(StreamRoute {
                                outbound: false,
                                ..p.host_routes[1]
                            })
                        );
                        received.push(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if !received.is_empty() {
                break;
            }
        }
        assert_eq!(received, [packet]);
    });
}
