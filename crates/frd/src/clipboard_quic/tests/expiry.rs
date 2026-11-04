//! A parked worker's first handoff and a later complete record have independent
//! original deadlines. Real UDP/TLS is used; native publication is not claimed.
use super::*;
use fr_core::clipboard::{Endpoint, Stamp};
use fr_wire::clipboard::{Message, encode};

fn message(f: &Fixture, body: Body<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 2048];
    let length = encode(
        Message {
            stamp: Stamp {
                id: 901,
                source: Endpoint::Controller,
                sequence: 1,
            },
            body,
        },
        f.peer.outgoing(),
        &f.peer.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

async fn send(f: &mut Fixture, body: Body<'_>) {
    let bytes = message(f, body);
    f.peer
        .send(
            &mut f.link.c,
            &f.cx,
            &bytes,
            clock(&f.cx) + 1_000_000,
            || true,
        )
        .unwrap();
    let until = clock(&f.cx) + 1_000_000;
    let expected = if f.bridge.shared.inbox.lock().unwrap().busy {
        Disposition::Blocked
    } else {
        Disposition::Consumed
    };
    let mut complete = false;
    while !complete {
        assert!(
            clock(&f.cx) < until,
            "clipboard record never reached its fixed slot"
        );
        f.link.drive(&f.cx).await;
        // Allocation after a header is not completion. Observe the actual
        // validated full record and the real handoff's admission result.
        f.bridge
            .lane
            .dispatch(
                &mut f.link.h,
                &f.cx,
                || true,
                |received| {
                    assert_eq!(received, bytes);
                    let (local, host) = f.bridge.admission().unwrap();
                    let admitted = f.bridge.shared.accept(received, local, host).unwrap();
                    assert_eq!(admitted, expected);
                    complete = true;
                    Ok(admitted)
                },
            )
            .unwrap();
    }
}

#[test]
fn earlier_blocked_record_expiry_fences_the_worker_and_reports_clipboard_only_expiry() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        send(
            &mut f,
            Body::Begin {
                total_bytes: 1024,
                chunks: 1,
            },
        )
        .await;
        assert!(f.bridge.shared.inbox.lock().unwrap().busy);
        let original_handoff = f
            .bridge
            .shared
            .inbound_until
            .load(std::sync::atomic::Ordering::Acquire);
        assert_ne!(original_handoff, 0);
        // The worker never consumes Begin. A larger Chunk stays complete in
        // transport, so its 2-second first-byte expiry precedes the first
        // handoff's 3-second bound. Neither deadline may be restarted.
        send(
            &mut f,
            Body::Chunk {
                index: 0,
                offset: 0,
                bytes: &[0x61; 1024],
            },
        )
        .await;
        let until = clock(&f.cx) + 1_000_000;
        while !f.peer.send_drained(&f.link.c).unwrap() {
            assert!(clock(&f.cx) < until, "native sender never drained");
            f.link.drive(&f.cx).await;
        }
        asupersync::time::sleep(f.cx.now(), Duration::from_millis(2100)).await;
        f.link.h.tick(&f.cx, || true).unwrap();
        assert!(f.bridge.lane.receive_expired(&f.link.h));
        assert!(
            clock(&f.cx) < original_handoff,
            "the earlier record bound must win"
        );
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Retired));
        assert_eq!(f.bridge.reason(), Some(Error::HandoffExpired));
        assert!(!f.bridge.shared.gate.is_open());
        assert!(f.bridge.shared.inbox.lock().unwrap().item.is_none());
        assert!(!f.link.h.is_closed());
        assert!(
            f.host
                .monitor()
                .deadline(HostInstant::from_micros(clock(&f.cx)))
                .is_ok()
        );
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Retired));
        assert_eq!(f.bridge.reason(), Some(Error::HandoffExpired));
        f.stop();
    });
}

#[test]
fn original_bridge_expires_only_a_complete_unsubmitted_handoff_and_keeps_input_live() {
    for executing in [false, true] {
        runtime().block_on(async {
            let mut f = Fixture::new().await;
            send(
                &mut f,
                Body::Begin {
                    total_bytes: 3,
                    chunks: 1,
                },
            )
            .await;
            let original = f
                .bridge
                .shared
                .inbound_until
                .load(std::sync::atomic::Ordering::Acquire);
            let bound = f
                .bridge
                .shared
                .inbound_bound
                .load(std::sync::atomic::Ordering::Acquire);
            // Exactly the worker's active-receive ownership transition. No
            // completion result means the bridge cannot assume a safe refusal.
            let _active =
                executing.then(|| f.bridge.shared.inbox.lock().unwrap().item.take().unwrap());
            let remaining = original.saturating_sub(clock(&f.cx));
            asupersync::time::sleep(f.cx.now(), Duration::from_micros(remaining + 1_000)).await;
            assert!(clock(&f.cx) >= original);
            assert!(!f.bridge.lane.receive_expired(&f.link.h));
            assert_eq!(
                f.bridge.maintain(&mut f.link.h),
                Ok(if executing {
                    State::Retired
                } else {
                    State::Active
                })
            );
            assert_eq!(
                f.bridge.reason(),
                executing.then_some(Error::HandoffExpired)
            );
            assert!(!f.link.h.is_closed());
            assert!(
                f.host
                    .monitor()
                    .deadline(HostInstant::from_micros(clock(&f.cx)))
                    .is_ok()
            );
            if !executing {
                assert!(f.bridge.permits_io());
                f.bridge.lane.check(&f.link.h).unwrap();
                let expired = {
                    let inbox = f.bridge.shared.inbox.lock().unwrap();
                    assert!(inbox.busy);
                    assert!(inbox.item.is_none());
                    assert!(inbox.result.is_none());
                    inbox.expired.expect("canonical refusal metadata")
                };
                assert_eq!(
                    f.bridge
                        .shared
                        .inbound_until
                        .load(std::sync::atomic::Ordering::Acquire),
                    original
                );
                assert_eq!(
                    f.bridge
                        .shared
                        .inbound_bound
                        .load(std::sync::atomic::Ordering::Acquire),
                    bound
                );
                // The original wire owner consumes the same rejection; no
                // replacement grant, native publication or fresh deadline.
                channel(&mut f.seed)
                    .expire_receive(expired, HostInstant::from_micros(clock(&f.cx)))
                    .unwrap();
                assert_eq!(channel(&mut f.seed).retained_bytes(), 0);
                assert!(f.bridge.shared.gate.is_open());
            }
            f.stop();
        });
    }
}
