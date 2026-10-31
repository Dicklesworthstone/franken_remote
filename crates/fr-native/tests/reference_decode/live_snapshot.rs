//! Live-request mailbox joined to the actual confined worker and X11 viewport.
//! This is local worker qualification, not a tailnet or authority-owner test.
use super::*;
use fr_client::robot::snapshot::{Channel, Error, Image, Request};
use fr_core::ids::RecoveryGeneration;
use fr_media::worker::presentation_snapshot::{Absent, Snapshot, Stamp};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

fn stamp(unit: &EncodedAccessUnit) -> Stamp {
    Stamp {
        frame: unit.frame(),
        configuration: unit.config_generation(),
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn submit(worker: &mut Worker, kind: Kind, unit: &EncodedAccessUnit) {
    let reply = worker.request(kind, worker::unit_payload(unit).unwrap());
    assert_eq!(
        reply.header.kind,
        if kind == Kind::Present {
            Kind::Presented
        } else {
            Kind::Decoded
        }
    );
    assert_eq!(reply.body(), unit.frame().as_raw().to_be_bytes());
}
fn decode_reply(reply: Record) -> Result<Snapshot, Error> {
    match reply.header.kind {
        Kind::PresentationPixels => Ok(Snapshot::decode(reply.into_body()).unwrap()),
        Kind::PresentationAbsent => Err(match Absent::decode(reply.body()).unwrap() {
            Absent::NoFrame | Absent::TooLarge => Error::Unavailable,
            Absent::Busy => Error::Busy,
            Absent::NotCurrent => Error::NotCurrent,
        }),
        kind => panic!("unexpected snapshot reply: {kind:?}"),
    }
}
fn read(worker: &mut Worker, expected: Stamp) -> Result<Snapshot, Error> {
    decode_reply(worker.request(Kind::ReadPresentation, expected.encode().to_vec()))
}
fn poll(request: &mut Request) -> Poll<Result<Image, Error>> {
    Pin::new(request).poll(&mut Context::from_waker(Waker::noop()))
}
fn image(request: &mut Request) -> Image {
    match poll(request) {
        Poll::Ready(Ok(image)) => image,
        Poll::Ready(Err(error)) => panic!("snapshot request refused: {error:?}"),
        Poll::Pending => panic!("snapshot request did not complete"),
    }
}
fn complete(channel: &mut Channel, worker: &mut Worker) {
    let work = channel.take().expect("one pending screenshot request");
    let reply = read(worker, work.stamp());
    work.complete(reply);
}

#[test]
fn static_native_and_fitted_exports_keep_one_global_permit_through_delivery() {
    for fit in [false, true] {
        let display = Display::start();
        let mut encoder = encoder();
        let first = encode(&mut encoder, 0);
        let (mut worker, mut ui, _) = configured(&display, &first, fit);
        submit(&mut worker, Kind::Present, &first);
        let expected = stamp(&first);
        let (mut channel, handle) = Channel::new();
        channel.publish(Some(expected));
        let other = handle.clone();
        let mut request = handle.request(expected).unwrap();
        let deadline = request.deadline();
        assert!(poll(&mut request).is_pending());
        let work = channel.take().unwrap();
        assert_eq!(work.stamp(), expected);
        assert_eq!(work.deadline(), deadline);
        assert_eq!(other.request(expected).err(), Some(Error::Busy));
        work.complete(read(&mut worker, expected));
        let held = image(&mut request);
        assert_eq!(held.deadline(), deadline);
        assert_eq!(held.snapshot().stamp(), expected);
        assert_eq!(held.snapshot().pixels(), ui.snapshot().unwrap().pixels());
        assert_eq!(
            (held.snapshot().width(), held.snapshot().height()),
            if fit { (96, 96) } else { (192, 128) }
        );
        drop(request);
        assert_eq!(other.request(expected).err(), Some(Error::Busy));
        assert!(channel.take().is_none());
        let original = held.snapshot().pixels().to_vec();
        drop(held);

        // A static viewport needs no new video, keyframe or decode completion.
        for _ in 0..3 {
            let mut request = other.request(expected).unwrap();
            complete(&mut channel, &mut worker);
            let held = image(&mut request);
            assert_eq!(held.snapshot().stamp(), expected);
            assert_eq!(held.snapshot().pixels(), original);
        }
        assert_eq!(
            worker.request(Kind::Poll, vec![]).header.kind,
            Kind::NeedInput
        );
        assert_eq!(ui.snapshot().unwrap().pixels(), original);
        worker.stop();
    }
}

#[test]
fn reference_history_and_new_presentation_never_relabel_an_old_request() {
    let display = Display::start();
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let (mut worker, mut ui, _) = configured(&display, &first, false);
    submit(&mut worker, Kind::Present, &first);
    let expected = stamp(&first);
    let original = ui.snapshot().unwrap().pixels().to_vec();
    let (mut channel, handle) = Channel::new();
    channel.publish(Some(expected));
    for wrong in [
        Stamp {
            frame: expected.frame.next().unwrap(),
            ..expected
        },
        Stamp {
            configuration: expected.configuration.next().unwrap(),
            ..expected
        },
        Stamp {
            recovery: expected.recovery.next().unwrap(),
            ..expected
        },
    ] {
        assert_eq!(handle.request(wrong).err(), Some(Error::NotCurrent));
        assert!(channel.take().is_none());
    }
    for id in 1..5 {
        let reference = encode(&mut encoder, id);
        submit(&mut worker, Kind::Decode, &reference);
        assert_eq!(
            handle.request(stamp(&reference)).err(),
            Some(Error::NotCurrent)
        );
        let mut request = handle.request(expected).unwrap();
        complete(&mut channel, &mut worker);
        let held = image(&mut request);
        assert_eq!(held.snapshot().stamp(), expected);
        assert_eq!(held.snapshot().pixels(), original);
    }

    let mut request = handle.request(expected).unwrap();
    let work = channel.take().unwrap();
    let old_pixels = read(&mut worker, work.stamp()).unwrap();
    let next = encode(&mut encoder, 5);
    submit(&mut worker, Kind::Present, &next);
    let current = stamp(&next);
    channel.publish(Some(current));
    work.complete(Ok(old_pixels));
    assert!(matches!(
        poll(&mut request),
        Poll::Ready(Err(Error::NotCurrent))
    ));
    drop(request);
    assert_eq!(handle.request(expected).err(), Some(Error::NotCurrent));
    let mut request = handle.request(current).unwrap();
    complete(&mut channel, &mut worker);
    let current = image(&mut request);
    assert_eq!(current.snapshot().pixels(), ui.snapshot().unwrap().pixels());
    assert_ne!(current.snapshot().pixels(), original);
    worker.stop();
}

#[test]
fn cancellation_during_native_export_discards_late_pixels_without_stopping_worker() {
    let display = Display::start();
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let (mut worker, mut ui, _) = configured(&display, &first, false);
    submit(&mut worker, Kind::Present, &first);
    let expected = stamp(&first);
    let (mut channel, handle) = Channel::new();
    channel.publish(Some(expected));
    let request = handle.request(expected).unwrap();
    let work = channel.take().unwrap();

    // Dispatch the real IPC operation, then abandon the local requester before
    // consuming its worker completion. No cancellation signal goes to the worker.
    let identity = Identity {
        epoch: 7,
        sequence: worker.sequence,
    };
    worker.sequence += 1;
    Record::new(
        Kind::ReadPresentation,
        identity,
        work.stamp().encode().to_vec(),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
    .write(&mut worker.input, &ProtocolLimits::ABSOLUTE)
    .unwrap();
    drop(request);
    assert_eq!(handle.request(expected).err(), Some(Error::Busy));
    let reply = worker
        .output
        .as_ref()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .expect("bounded native export reply");
    assert_eq!(reply.header.identity, identity);
    work.complete(decode_reply(reply));

    // The cancelled operation's slot is released only after its native work
    // completes. A fresh request reuses the same original live worker.
    let mut request = handle.request(expected).unwrap();
    complete(&mut channel, &mut worker);
    let held = image(&mut request);
    assert_eq!(held.snapshot().stamp(), expected);
    assert_eq!(held.snapshot().pixels(), ui.snapshot().unwrap().pixels());
    drop(held);
    let next = encode(&mut encoder, 1);
    submit(&mut worker, Kind::Present, &next);
    let current = stamp(&next);
    channel.publish(Some(current));
    let mut request = handle.request(current).unwrap();
    complete(&mut channel, &mut worker);
    assert_eq!(
        image(&mut request).snapshot().pixels(),
        ui.snapshot().unwrap().pixels()
    );
    worker.stop();
}
