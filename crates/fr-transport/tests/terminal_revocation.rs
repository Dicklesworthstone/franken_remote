//! Real UDP/TLS/QUIC terminal reporting. The grant/fence is a local fixture;
//! these tests make no native-input, Tailscale, or cleanup-success claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;
#[allow(dead_code)]
#[path = "support/terminal.rs"]
mod terminal;

use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    authority::Binding,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, Reason, Revoked},
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{both, clock, runtime};

use crate::terminal::{Pair, binding, host as pair, report};
use crate::terminal::{bulk, queued_video};

#[test]
fn terminal_report_crosses_real_quic_but_unstaged_media_does_not() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        pair.local
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(pair.local.usage().retained_send_records, 1);
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(
            pair.local.is_closed(),
            "ordinary traffic must stop before polling"
        );
        assert_eq!(pair.local.usage().retained_send_records, 0);
        let (ended, received) =
            terminal::collect(&cx, &mut pair, end, terminal::decode_revocation).await;
        assert_eq!(ended, Ok(()));
        assert_eq!(received, [report()]);
        assert_eq!(
            pair.local.send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::Closed)
        );
    });
}

#[test]
fn dropping_an_unpolled_report_cannot_leave_the_original_owner_live() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.local.is_closed());
        drop(end);
        assert_eq!(pair.local.tick(&cx, || true), Err(Error::Closed));
        assert_eq!(
            pair.peer
                .receive(&cx, || true, |_, _| panic!("unpolled report sent")),
            Ok(0)
        );
    });
}

#[test]
fn wrong_connection_proof_does_not_close_a_replacement() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let foreign = pair.peer.binding();
        assert_eq!(
            pair.local
                .close_with_revocation(&cx, &foreign, pair.control, binding(), report())
                .await,
            Err(Error::WrongRoute)
        );
        assert!(!pair.local.is_closed());
        assert_eq!(pair.local.tick(&cx, || true), Ok(()));
    });
}

#[test]
fn matched_owner_closes_on_wrong_route_and_malformed_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for malformed in [false, true] {
            let mut pair = pair(&cx).await;
            let original = pair.local.binding();
            let route = if malformed { pair.control } else { pair.bulk };
            let notice = if malformed {
                Revoked {
                    lease: InputLeaseId::from_raw(0),
                    ..report()
                }
            } else {
                report()
            };
            let end = pair
                .local
                .close_with_revocation(&cx, &original, route, binding(), notice);
            assert!(pair.local.is_closed());
            assert_eq!(
                end.await,
                Err(if malformed {
                    Error::Malformed
                } else {
                    Error::WrongRoute
                })
            );
        }
    });
}

#[test]
fn immutable_ingress_guard_and_parent_cancellation_survive_handoff() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let allowed = Arc::new(AtomicBool::new(true));
        let guard = allowed.clone();
        pair.local
            .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
            .unwrap();
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        allowed.store(false, Ordering::Release);
        assert_eq!(end.await, Err(Error::Unauthorized));
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        cx.cancel_fast(CancelKind::User);
        assert_eq!(end.await, Err(Error::Cancelled));
    });
}

#[test]
fn delayed_first_poll_does_not_restart_terminal_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
        assert_eq!(end.await, Err(Error::Expired));
    });
}

#[test]
fn native_retained_stream_payload_is_refused_instead_of_flushed_after_revocation() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        pair.local
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        pair.local
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        assert!(pair.local.usage().retained_send_records > 0);
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.local.is_closed());
        assert_eq!(end.await, Err(Error::Backpressure));
    });
}

/// A queued DATAGRAM is unreliable media no peer is owed after the fence: it
/// is discarded unsent, and the report is delivered instead of refused.
#[test]
fn native_queued_datagram_is_discarded_without_a_post_fence_flush() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        queued_video(&mut pair, &cx);
        let original = pair.local.binding();
        let end =
            pair.local
                .close_with_revocation(&cx, &original, pair.control, binding(), report());
        assert!(pair.local.is_closed());
        // `collect` fails on any record outside the original control route.
        let (ended, received) =
            terminal::collect(&cx, &mut pair, end, terminal::decode_revocation).await;
        assert_eq!(ended, Ok(()));
        assert_eq!(received, [report()]);
        // A DATAGRAM may trail the report's packet; give it one more turn.
        pair.peer
            .drive(&cx, Duration::from_millis(10), || true)
            .await
            .unwrap();
        assert_eq!(
            pair.peer
                .receive(&cx, || true, |_, _| panic!("stale datagram leaked")),
            Ok(0)
        );
    });
}

#[path = "terminal_revocation/deferred.rs"]
mod deferred;

#[path = "terminal_revocation/closed.rs"]
mod closed;

#[path = "terminal_revocation/closed_deferred.rs"]
mod closed_deferred;

#[path = "terminal_revocation/exchange.rs"]
mod exchange;
