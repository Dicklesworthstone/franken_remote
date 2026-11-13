//! Real native DATAGRAM generation and partially staged reliable output.
//! Queue emptiness is observable here; protected-socket backpressure is not
//! manufactured. The local input fence and terminal reason remain fixtures.
use super::*;
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    task::Poll,
};

async fn observe_report(pair: &mut Pair, cx: &Cx, reporting: RevocationReport) {
    let done = Cell::new(false);
    let mut received = Vec::new();
    let (result, ()) = Box::pin(support::both(
        async {
            let result = reporting.finish().await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                pair.peer
                    .drive_io_once(cx, Duration::from_millis(5))
                    .await
                    .unwrap();
                assert!(pair.peer.connection_mut().recv_datagram().is_none());
                if let Ok(bytes) = pair.peer.connection_mut().read_stream(
                    cx,
                    pair.control.stream,
                    pair.control.maximum,
                ) {
                    received.extend_from_slice(&bytes);
                }
            }
        },
    ))
    .await;
    assert_eq!(result, Some(Ok(())));
    assert!(!pair.peer.connection().close_was_peer_initiated());
    assert_eq!(terminal::decode_revocation(&received), terminal::report());
}

#[test]
fn native_datagram_history_requires_a_completed_owned_turn_before_a_report() {
    for complete_owned_turn in [false, true] {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut pair = Box::pin(with_native_history(&cx)).await;
            let (reporting, fenced) = arm(&mut pair, &cx);
            if complete_owned_turn {
                pair.local
                    .drive(&cx, Duration::ZERO, || true)
                    .await
                    .unwrap();
            }
            pair.local.close();
            assert!(fenced.load(Ordering::Acquire));
            assert!(pair.local.is_closed());
            if complete_owned_turn {
                Box::pin(observe_report(&mut pair, &cx, reporting)).await;
            } else {
                assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
                observe_close(&mut pair, &cx, 0x4652_0201).await;
            }
            assert_eq!(pair.local.tick(&cx, || true), Err(Error::Closed));
        }));
    }
}

/// A DATAGRAM still queued at the fence is dropped unsent: the registered
/// report is delivered instead of the close packet, and the peer never receives
/// the datagram.
#[test]
fn a_queued_datagram_is_discarded_and_the_registered_report_is_delivered() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut pair = Box::pin(pair(&cx)).await;
        let (reporting, fenced) = arm(&mut pair, &cx);
        queued_video(&mut pair, &cx);
        pair.local.close();
        assert!(fenced.load(Ordering::Acquire));
        Box::pin(observe_report(&mut pair, &cx, reporting)).await;
        // A DATAGRAM may trail the report's packet; give it one more turn.
        pair.peer
            .drive_io_once(&cx, Duration::from_millis(10))
            .await
            .unwrap();
        assert!(
            pair.peer.connection_mut().recv_datagram().is_none(),
            "a discarded datagram was sent"
        );
    }));
}

fn queue_large_recovery(pair: &mut Pair, cx: &Cx) -> usize {
    let bytes = terminal::recovery_bytes(60_000);
    let n = bytes.len();
    pair.local
        .send(
            cx,
            Route::Stream(pair.bulk),
            &bytes[..n],
            clock(cx) + 2_000_000,
            || true,
        )
        .unwrap();
    n
}

fn incomplete_payload_native_io(drop_pending: bool) {
    let rt = runtime();
    let session = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(Box::pin(async {
        let mut pair = Box::pin(pair(&session)).await;
        let (reporting, fenced) = arm(&mut pair, &cleanup);
        let original = pair.local.binding();
        let record_bytes = queue_large_recovery(&mut pair, &session);
        // At most one bounded prefix turn enters native output; the full
        // recovery record is larger. No peer consumes or ACKs it yet.
        pair.local
            .drive(&session, Duration::ZERO, || true)
            .await
            .unwrap();
        assert_eq!(pair.local.usage().retained_send_records, 1);
        let mut exercised = false;
        for _ in 0..16 {
            let mut operation = Box::pin(pair.local.drive(
                &session,
                Duration::from_millis(100),
                || true,
            ));
            let polled = poll_fn(|task| Poll::Ready(operation.as_mut().poll(task))).await;
            match polled {
                Poll::Ready(result) => assert_eq!(result, Ok(())),
                Poll::Pending => {
                    if drop_pending {
                        // Drop the owning Box, not just a Pin<&mut Future>.
                        drop(operation);
                    } else {
                        session.cancel_fast(CancelKind::User);
                        assert_eq!(operation.await, Err(Error::Cancelled));
                    }
                    exercised = true;
                    break;
                }
            }
        }
        assert!(exercised, "must exercise genuinely pending native I/O");
        assert!(fenced.load(Ordering::Acquire));
        assert!(!cleanup.is_cancel_requested());
        assert!(pair.local.is_closed());
        assert!(pair.local.is_bound_to(&original));
        assert_eq!(pair.local.usage().retained_send_records, 0);
        assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
        observe_close(&mut pair, &cleanup, 0x4652_0201).await;
        // An earlier prefix may already have crossed UDP. Closing cannot
        // retract it, but must never finish or replay its original record.
        let received = pair
            .peer
            .connection()
            .inner()
            .streams()
            .stream(pair.bulk.stream)
            .map_or(0, |stream| stream.recv_offset);
        assert!(received < u64::try_from(record_bytes).unwrap());
        let control = pair
            .peer
            .connection()
            .inner()
            .streams()
            .stream(pair.control.stream)
            .map_or(0, |stream| stream.recv_offset);
        assert_eq!(control, 0, "a close packet must not claim report delivery");
        assert_eq!(pair.local.tick(&cleanup, || true), Err(Error::Closed));
    }));
}

#[test]
fn cancelled_payload_holding_io_fences_then_sends_only_the_close_packet() {
    incomplete_payload_native_io(false);
}

#[test]
fn dropped_payload_holding_io_fences_then_sends_only_the_close_packet() {
    incomplete_payload_native_io(true);
}
