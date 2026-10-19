//! Real native DATAGRAM generation and partially staged reliable output.
//! Queue emptiness is observable here; protected-socket backpressure is not
//! manufactured. The local input fence and terminal reason remain fixtures.
use super::*;
use asupersync::bytes::Bytes;
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    task::Poll,
};

async fn with_native_history(cx: &Cx) -> Pair {
    let (client, server) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    let earlier = Bytes::from_static(b"native output before the record owner");
    server
        .connection_mut()
        .send_datagram(cx, earlier.clone())
        .unwrap();
    server.flush(cx).await.unwrap();
    assert_eq!(
        server
            .connection()
            .inner()
            .pending_outbound_datagram_count(),
        0
    );
    assert!(server.connection().inner().datagrams_sent() > 0);
    // Observe this already-authorized payload before arming the new owner's
    // fence. History is not proof that an arbitrary incoming native owner has
    // finished its last UDP send; only a completed owned turn can supply that.
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "the native fixture did not cross UDP");
        client
            .drive_io_once(cx, Duration::from_millis(5))
            .await
            .unwrap();
        if let Some(bytes) = client.connection_mut().recv_datagram() {
            assert_eq!(bytes, earlier);
            break;
        }
    }
    from_native_pair(cx, client, server)
}

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
                pair.client
                    .drive_io_once(cx, Duration::from_millis(5))
                    .await
                    .unwrap();
                assert!(pair.client.connection_mut().recv_datagram().is_none());
                if let Ok(bytes) = pair.client.connection_mut().read_stream(
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
    assert!(!pair.client.connection().close_was_peer_initiated());
    assert_eq!(
        fr_wire::lease_revoked::decode(
            &received,
            binding(),
            InputLeaseId::from_raw(2),
            &ProtocolLimits::ABSOLUTE,
            fr_wire::input::InputDirection::HostToViewer,
            fr_wire::input::InputDelivery::Reliable,
        ),
        Ok(fr_wire::lease_revoked::Revoked {
            lease: InputLeaseId::from_raw(2),
            reason: Reason::LocalRevoke,
            cleanup: fr_wire::lease_revoked::CleanupStage::Fenced,
            effects: fr_wire::lease_revoked::EffectStage::Unknown,
        })
    );
}

#[test]
fn native_datagram_history_requires_a_completed_owned_turn_before_a_report() {
    for complete_owned_turn in [false, true] {
        runtime().block_on(Box::pin(async {
            let cx = Cx::current().unwrap();
            let mut pair = Box::pin(with_native_history(&cx)).await;
            let (reporting, fenced) = arm(&mut pair, &cx);
            if complete_owned_turn {
                pair.server
                    .drive(&cx, Duration::ZERO, || true)
                    .await
                    .unwrap();
            }
            pair.server.close();
            assert!(fenced.load(Ordering::Acquire));
            assert!(pair.server.is_closed());
            if complete_owned_turn {
                Box::pin(observe_report(&mut pair, &cx, reporting)).await;
            } else {
                assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
                observe_close(&mut pair, &cx, 0x4652_0201).await;
            }
            assert_eq!(pair.server.tick(&cx, || true), Err(Error::Closed));
        }));
    }
}

fn queue_large_recovery(pair: &mut Pair, cx: &Cx) -> usize {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let mut bytes = vec![0; 65536];
    let data = vec![99; 60_000];
    let n = fr_wire::encode_recovery(
        fr_wire::RecoveryChunk {
            frame: 0,
            total_bytes: 60_000,
            offset: 0,
            capture_micros: 1,
            bytes: &data,
        },
        pair.bulk.binding,
        &limits,
        &mut bytes,
    )
    .unwrap();
    pair.server
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
        let original = pair.server.binding();
        let record_bytes = queue_large_recovery(&mut pair, &session);
        // At most one bounded prefix turn enters native output; the full
        // recovery record is larger. No peer consumes or ACKs it yet.
        pair.server
            .drive(&session, Duration::ZERO, || true)
            .await
            .unwrap();
        assert_eq!(pair.server.usage().retained_send_records, 1);
        let mut exercised = false;
        for _ in 0..16 {
            let mut operation = Box::pin(pair.server.drive(
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
        assert!(pair.server.is_closed());
        assert!(pair.server.is_bound_to(&original));
        assert_eq!(pair.server.usage().retained_send_records, 0);
        assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
        observe_close(&mut pair, &cleanup, 0x4652_0201).await;
        // An earlier prefix may already have crossed UDP. Closing cannot
        // retract it, but must never finish or replay its original record.
        let received = pair
            .client
            .connection()
            .inner()
            .streams()
            .stream(pair.bulk.stream)
            .map_or(0, |stream| stream.recv_offset);
        assert!(received < u64::try_from(record_bytes).unwrap());
        let control = pair
            .client
            .connection()
            .inner()
            .streams()
            .stream(pair.control.stream)
            .map_or(0, |stream| stream.recv_offset);
        assert_eq!(control, 0, "a close packet must not claim report delivery");
        assert_eq!(pair.server.tick(&cleanup, || true), Err(Error::Closed));
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
