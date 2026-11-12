//! The actual UDP/TLS bridge and worker with an explicitly recording native
//! fixture. No real OS publication or live clipboard qualification is claimed.
use super::*;
use fr_core::clipboard::{ClipboardSink, Endpoint, Publication, Receipt, Stamp};
use fr_wire::clipboard::{
    Message, encode,
    session::synchronize::{NativeChange, NativeChanges, NativeText},
};
use std::sync::Mutex;

#[derive(Default)]
struct Effects {
    published: Vec<Stamp>,
    closed: bool,
    local: Option<&'static str>,
    revision: u64,
    changed: bool,
    reading: bool,
}
struct Recording(Arc<Mutex<Effects>>);
struct LocalText(&'static str);
impl NativeText for LocalText {
    fn text(&self) -> &'static str {
        self.0
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
        let mut effects = self.0.lock().unwrap();
        effects.published.push(stamp);
        effects.reading = false;
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Recording {
    type Text = LocalText;
    type Error = ();
    fn watch(&mut self) -> Result<u64, ()> {
        Ok(0)
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        let mut effects = self.0.lock().unwrap();
        let latest = std::mem::take(&mut effects.changed).then_some(NativeChange {
            revision: effects.revision,
            has_selection: effects.local.is_some(),
            origin: None,
        });
        Ok(NativeChanges {
            latest,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        self.0.lock().unwrap().revision
    }
    fn prepare_for_revision(
        &mut self,
        _: &str,
        _: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        assert_eq!(revision, self.0.lock().unwrap().revision);
        Ok(())
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.0.lock().unwrap().reading = true;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<LocalText>, ()> {
        let mut effects = self.0.lock().unwrap();
        if std::mem::take(&mut effects.reading) {
            Ok(effects.local.map(LocalText))
        } else {
            Ok(None)
        }
    }
    fn cancel_read(&mut self) {
        self.0.lock().unwrap().reading = false;
    }
    fn suspend(&mut self) -> Result<(), ()> {
        Ok(())
    }
    fn close(&mut self) {
        let mut effects = self.0.lock().unwrap();
        effects.closed = true;
        effects.local = None;
        effects.reading = false;
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

async fn expire_retained_send(
    q: &mut QuicRecords,
    cx: &Cx,
    original_send_by: u64,
    mut authorize: impl FnMut() -> bool,
) {
    assert!(q.usage().retained_send_records > 0);
    assert_eq!(q.usage().critical_send_records, 0);
    let until = original_send_by.max(clock(cx) + quic::MAX_DELIVERY_ALLOWANCE_US) + 100_000;
    // Keep native custody settled and keep the original authorization gate.
    // The retained deadline is max(original send-by, admission + path allowance);
    // this test guard only bounds the wait and cannot renew either obligation.
    while q.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "original clipboard retention did not expire");
        asupersync::time::sleep(cx.now(), Duration::from_millis(5)).await;
        q.tick(cx, &mut authorize).unwrap();
    }
    assert_eq!(q.usage().retained_send_upper_bound, 0);
    assert!(!q.is_closed());
}

async fn original_control_progress(link: &mut attachment::Link, cx: &Cx) {
    use fr_wire::{
        authority::{self, Message, Scope},
        input::{InputDelivery, InputDirection},
    };
    let mut bytes = [0_u8; authority::MAX_AUTHORITY_BYTES];
    let length = authority::encode(
        Message::Response {
            scope: Scope::Observation,
            nonce: 903,
        },
        authority::Binding {
            channel: link.cr.outbound.binding,
            session: attachment::parent().remote_session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let until = clock(cx) + 500_000;
    link.c
        .send(
            cx,
            quic::Route::Stream(link.cr.outbound),
            &bytes[..length],
            until,
            || true,
        )
        .unwrap();
    let mut received = false;
    while !received {
        assert!(clock(cx) < until, "original critical route stopped");
        link.drive(cx).await;
        link.h
            .receive_ready(
                cx,
                || true,
                |route| route == quic::Route::Stream(link.hr.inbound),
                |_, body| {
                    assert_eq!(body, &bytes[..length]);
                    received = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
}

// Keep the same native receipt, worker and connection in every phase.
#[allow(clippy::too_many_lines)]
async fn send_expiry_after_publication(from_host: bool, staged: bool) {
    let Fixture {
        cx,
        mut link,
        mut bridge,
        seed,
        mut peer,
        host,
        _input: input,
        ..
    } = Fixture::new().await;
    let access = seed.native_access();
    let effects = Arc::new(Mutex::new(Effects::default()));
    let native = effects.clone();
    let mut worker = seed.open(|| Ok(Recording(native))).unwrap();
    let actual_receipt = publish_first(&mut link, &cx, &bridge, &peer, &mut worker).await;
    quiet(&mut link, &cx).await;
    assert_eq!(effects.lock().unwrap().published, [FIRST]);

    if from_host {
        // A new native selection follows the earlier publication. Queueing it
        // before that publication would correctly supersede its old permit.
        {
            let mut native = effects.lock().unwrap();
            native.local = Some("a later local selection");
            native.revision = 1;
            native.changed = true;
        }
        let until = clock(&cx) + 500_000;
        while bridge.shared.outbox.lock().unwrap().item.is_none() {
            assert!(clock(&cx) < until, "original native read did not produce Begin");
            worker.step(|| Ok(903)).unwrap();
        }
        assert_eq!(bridge.service(&mut link.h, || true), Ok(State::Active));
        assert_eq!(bridge.inflight.iter().flatten().count(), 1);
        assert!(bridge.pending.is_none());
        let original_until = bridge.inflight.iter().flatten().next().unwrap().1;
        // The actual native sender supplies its ORIGINAL item deadline as
        // send-by. It is not a shorter transport deadline in this path.
        assert!(original_until > clock(&cx) + quic::MAX_DELIVERY_ALLOWANCE_US);
        let item_cap = HostInstant::from_micros(original_until);
        assert!(access.check(Some(item_cap)));
        if staged {
            link.h
                .drive(&cx, Duration::ZERO, || bridge.permits_io())
                .await
                .unwrap();
        }
        expire_retained_send(&mut link.h, &cx, original_until, || bridge.permits_io()).await;
        assert!(bridge.lane.send_expired(&link.h));
        // Both original bounds have now elapsed. The strict I/O callback may
        // request retirement; only the recorded transport cause identifies it
        // as SendExpired. No expired item permit becomes valid again.
        let now = HostInstant::from_micros(clock(&cx));
        for (permit, until) in bridge.inflight.iter().flatten() {
            assert_eq!(
                permit.check_operation(now),
                Err(SessionError::Clipboard(fr_core::clipboard::Error::Expired))
            );
            assert_eq!(*until, original_until);
        }
        assert!(!access.check(Some(item_cap)));
    } else {
        let bytes = message(
            &peer,
            Stamp {
                id: 904,
                sequence: 2,
                ..FIRST
            },
            Body::Begin {
                total_bytes: 3,
                chunks: 1,
            },
        );
        // This peer is an explicit transport fixture with a short fixed
        // admission deadline; no native clipboard source is manufactured.
        let send_by = clock(&cx) + 100_000;
        peer.send(&mut link.c, &cx, &bytes, send_by, || true)
            .unwrap();
        if staged {
            link.c.drive(&cx, Duration::ZERO, || true).await.unwrap();
        }
        expire_retained_send(&mut link.c, &cx, send_by, || true).await;
        assert!(peer.send_expired(&link.c));
        assert!(access.check(None));
    }

    // Let only the original peer learn the terminal reset. Even a staged
    // Begin cannot bypass the uncollected publication receipt or reach the OS.
    let until = clock(&cx) + 1_000_000;
    while !bridge.lane.send_expired(&link.h) || !peer.send_expired(&link.c) {
        assert!(clock(&cx) < until, "original send-expiry reset did not arrive");
        link.drive(&cx).await;
    }
    assert_eq!(bridge.maintain(&mut link.h), Ok(State::Retired));
    assert_eq!(bridge.reason(), Some(Error::SendExpired));
    assert!(bridge.pending.is_none());
    assert!(bridge.inflight.iter().all(Option::is_none));
    assert!(bridge.shared.outbox.lock().unwrap().item.is_none());
    assert!(bridge.shared.inbox.lock().unwrap().item.is_none());
    assert!(!bridge.shared.gate.is_open());
    assert!(!access.check(None));
    assert_eq!(worker.step(|| Ok(905)), Err(Error::Closed));
    worker.close();
    assert!(effects.lock().unwrap().closed);
    assert_eq!(effects.lock().unwrap().published, [FIRST]);
    assert_eq!(bridge.take_received().unwrap(), Some(actual_receipt));
    assert!(bridge.take_received().unwrap().is_none());
    assert_eq!(bridge.service(&mut link.h, || true), Ok(State::Retired));
    assert_eq!(bridge.reason(), Some(Error::SendExpired));
    assert_eq!(worker.step(|| Ok(905)), Err(Error::Closed));
    assert_eq!(effects.lock().unwrap().published, [FIRST]);

    assert!(
        host.monitor()
            .deadline(HostInstant::from_micros(clock(&cx)))
            .is_ok()
    );
    input.0.completed_on(&link.h).unwrap();
    input.1.completed_on(&link.c).unwrap();
    original_control_progress(&mut link, &cx).await;
    assert!(!link.h.is_closed() && !link.c.is_closed());
    bridge.retire(&mut link.h).unwrap();
    peer.retire(&mut link.c, &cx).unwrap();
    assert_eq!(bridge.reason(), Some(Error::SendExpired));
}

#[test]
fn queued_clipboard_send_expiry_preserves_native_receipt_and_original_session() {
    runtime().block_on(async {
        for from_host in [false, true] {
            Box::pin(send_expiry_after_publication(from_host, false)).await;
        }
    });
}

#[test]
fn staged_clipboard_send_expiry_preserves_native_receipt_and_original_session() {
    runtime().block_on(async {
        for from_host in [false, true] {
            Box::pin(send_expiry_after_publication(from_host, true)).await;
        }
    });
}
