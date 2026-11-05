//! The actual UDP/TLS bridge and worker with an explicitly recording native
//! fixture. No real OS publication or live clipboard qualification is claimed.
use super::*;
use fr_core::clipboard::{ClipboardSink, Endpoint, Publication, Receipt, Stamp};
use fr_wire::clipboard::{
    Message, encode,
    session::synchronize::{NativeChanges, NativeText},
};
use std::sync::Mutex;

#[derive(Default)]
struct Effects {
    published: Vec<Stamp>,
    closed: bool,
}
struct Recording(Arc<Mutex<Effects>>);
struct EmptyText;
impl NativeText for EmptyText {
    fn text(&self) -> &str {
        ""
    }
    fn origin(&self) -> Option<Stamp> {
        None
    }
}
impl ClipboardSink for Recording {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, _: &str, stamp: Stamp) -> Publication {
        self.0.lock().unwrap().published.push(stamp);
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Recording {
    type Text = EmptyText;
    type Error = ();
    fn watch(&mut self) -> Result<u64, ()> {
        Ok(0)
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        Ok(NativeChanges {
            latest: None,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        0
    }
    fn prepare_for_revision(
        &mut self,
        _: &str,
        _: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        assert_eq!(revision, 0);
        Ok(())
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<EmptyText>, ()> {
        Ok(None)
    }
    fn cancel_read(&mut self) {}
    fn suspend(&mut self) -> Result<(), ()> {
        Ok(())
    }
    fn close(&mut self) {
        self.0.lock().unwrap().closed = true;
    }
}

fn message(peer: &ClipboardChannel, stamp: Stamp, body: Body<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 8192];
    let n = encode(
        Message { stamp, body },
        peer.outgoing(),
        &peer.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

async fn deliver(
    link: &mut attachment::Link,
    cx: &Cx,
    bridge: &Bridge,
    peer: &ClipboardChannel,
    bytes: &[u8],
    expected: Disposition,
) {
    peer.send(&mut link.c, cx, bytes, clock(cx) + 4_000_000, || true)
        .unwrap();
    let until = clock(cx) + 1_000_000;
    let mut arrived = false;
    while !arrived {
        assert!(clock(cx) < until);
        link.drive(cx).await;
        bridge
            .lane
            .dispatch(
                &mut link.h,
                cx,
                || true,
                |actual| {
                    assert_eq!(actual, bytes);
                    let (local, host) = bridge.admission().unwrap();
                    let disposition = bridge.shared.accept(actual, local, host).unwrap();
                    assert_eq!(disposition, expected);
                    arrived = true;
                    Ok(disposition)
                },
            )
            .unwrap();
    }
}

async fn quiet(link: &mut attachment::Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while link.h.usage().retained_send_records != 0 || link.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until);
        link.drive(cx).await;
    }
}

const FIRST: Stamp = Stamp {
    id: 901,
    source: Endpoint::Controller,
    sequence: 1,
};

async fn publish_first(
    link: &mut attachment::Link,
    cx: &Cx,
    bridge: &Bridge,
    peer: &ClipboardChannel,
    worker: &mut Worker<Recording>,
) -> Received {
    for body in [
        Body::Begin {
            total_bytes: 3,
            chunks: 1,
        },
        Body::Chunk {
            index: 0,
            offset: 0,
            bytes: b"old",
        },
        Body::Commit { total_bytes: 3 },
    ] {
        deliver(
            link,
            cx,
            bridge,
            peer,
            &message(peer, FIRST, body),
            Disposition::Consumed,
        )
        .await;
        worker.step(|| Ok(999)).unwrap();
    }
    Received::Consumed(Some(Receipt {
        stamp: FIRST,
        publication: Publication::SubmittedToOs,
    }))
}

async fn park_next(
    link: &mut attachment::Link,
    cx: &Cx,
    bridge: &mut Bridge,
    peer: &ClipboardChannel,
    worker: &mut Worker<Recording>,
    retain_receipt: bool,
) {
    let next = Stamp {
        id: 902,
        sequence: 2,
        ..FIRST
    };
    deliver(
        link,
        cx,
        bridge,
        peer,
        &message(
            peer,
            next,
            Body::Begin {
                total_bytes: 7800,
                chunks: 1,
            },
        ),
        if retain_receipt {
            Disposition::Blocked
        } else {
            Disposition::Consumed
        },
    )
    .await;
    quiet(link, cx).await;
    if !retain_receipt {
        // The real worker owns the next Begin, but never receives a complete
        // Chunk or Commit. Its original item storage must die with this lane.
        worker.step(|| Ok(999)).unwrap();
        let bytes = message(
            peer,
            next,
            Body::Chunk {
                index: 0,
                offset: 0,
                bytes: &[0x61; 7800],
            },
        );
        peer.send(&mut link.c, cx, &bytes, clock(cx) + 4_000_000, || true)
            .unwrap();
        link.c.drive(cx, Duration::ZERO, || true).await.unwrap();
        for _ in 0..3 {
            link.h
                .drive(cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            assert_eq!(bridge.service(&mut link.h, || true), Ok(State::Active));
        }
        assert!(link.h.usage().framed_capacity >= 7800);
        assert!(bridge.shared.inbox.lock().unwrap().item.is_none());
    }
}

async fn expire_after_publication(retain_receipt: bool) {
    let Fixture {
        cx,
        mut link,
        mut bridge,
        seed,
        mut peer,
        host,
        _input,
        ..
    } = Fixture::new().await;
    let effects = Arc::new(Mutex::new(Effects::default()));
    let native = effects.clone();
    let mut worker = seed.open(|| Ok(Recording(native))).unwrap();
    let actual_receipt = publish_first(&mut link, &cx, &bridge, &peer, &mut worker).await;
    if !retain_receipt {
        assert_eq!(bridge.take_received().unwrap(), Some(actual_receipt));
    }
    assert_eq!(effects.lock().unwrap().published, [FIRST]);
    park_next(
        &mut link,
        &cx,
        &mut bridge,
        &peer,
        &mut worker,
        retain_receipt,
    )
    .await;
    asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
    assert_eq!(bridge.service(&mut link.h, || true), Ok(State::Retired));
    assert_eq!(bridge.reason(), Some(Error::ReceiveExpired));
    assert!(!link.h.is_closed());
    assert!(!bridge.shared.gate.is_open());
    assert!(bridge.shared.inbox.lock().unwrap().item.is_none());
    assert_eq!(worker.step(|| Ok(999)), Err(Error::Closed));
    worker.close();
    assert!(effects.lock().unwrap().closed);
    assert_eq!(effects.lock().unwrap().published, [FIRST]);
    assert_eq!(
        bridge.take_received().unwrap(),
        retain_receipt.then_some(actual_receipt)
    );
    assert!(bridge.take_received().unwrap().is_none());
    assert!(
        host.monitor()
            .deadline(HostInstant::from_micros(clock(&cx)))
            .is_ok()
    );
    let until = clock(&cx) + 1_000_000;
    while !peer.receive_expired(&link.c) {
        assert!(clock(&cx) < until);
        link.drive(&cx).await;
    }
    assert_eq!(bridge.service(&mut link.h, || true), Ok(State::Retired));
    assert_eq!(bridge.reason(), Some(Error::ReceiveExpired));
    assert_eq!(worker.step(|| Ok(999)), Err(Error::Closed));
    assert_eq!(effects.lock().unwrap().published, [FIRST]);
    bridge.retire(&mut link.h).unwrap();
    peer.retire(&mut link.c, &cx).unwrap();
}

#[test]
fn partial_body_expiry_fences_the_original_worker_without_replaying_an_item() {
    runtime().block_on(Box::pin(expire_after_publication(false)));
}

#[test]
fn later_receive_expiry_preserves_the_original_uncollected_native_receipt() {
    runtime().block_on(Box::pin(expire_after_publication(true)));
}
