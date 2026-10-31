use super::*;
use fr_client::robot::snapshot::Channel;
use fr_core::ids::{CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration};
use fr_media::{
    access_unit::FrameId,
    worker::presentation_snapshot::{Snapshot, Stamp},
};
use fr_wire::display::Display;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Instant,
};
fn stamp() -> Stamp {
    Stamp {
        frame: FrameId::from_raw(7),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn image() -> Snapshot {
    Snapshot::decode(Snapshot::encode(stamp(), 2, 2, &[0, 0, 0, 255].repeat(4)).unwrap()).unwrap()
}
fn ready() -> (Publisher, Channel, Handle) {
    let publisher = Publisher::default();
    publisher
        .update(|state| {
            state.begin(1)?;
            state.authenticated()?;
            state.ready(
                Display {
                    handle: 17,
                    geometry: DisplayGeometryGeneration::INITIAL,
                    x: 0,
                    y: 0,
                    pixel_width: 800,
                    pixel_height: 600,
                    logical_width: 800,
                    logical_height: 600,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                },
                7,
                Evidence::SubmittedToCompositor,
                Instant::now(),
            )
        })
        .unwrap();
    let (channel, handle) = Channel::new();
    channel.publish(Some(stamp()));
    publisher.bind_snapshot(handle.clone()).unwrap();
    (publisher, channel, handle)
}
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
#[test]
fn capture_retains_one_export_permit_and_retirement_cannot_seal_it() {
    let (publisher, channel, handle) = ready();
    let mut capture = Box::pin(publisher.capture());
    assert!(poll(capture.as_mut()).is_pending());
    channel.take().unwrap().complete(Ok(image()));
    let Poll::Ready(Ok((metadata, image))) = poll(capture.as_mut()) else {
        panic!("original image must complete");
    };
    assert_eq!(publisher.check_export(&metadata), Ok(()));
    assert!(matches!(
        handle.request(stamp()),
        Err(fr_client::robot::snapshot::Error::Busy)
    ));
    publisher.retire(Phase::Cleaning);
    assert_eq!(publisher.check_export(&metadata), Err(Refusal::NotCurrent));
    drop(image);
    assert!(handle.request(stamp()).is_ok());
}
#[test]
fn changed_receipt_never_substitutes_a_new_frame_for_requested_pixels() {
    let (publisher, channel, _) = ready();
    let mut capture = Box::pin(publisher.capture());
    assert!(poll(capture.as_mut()).is_pending());
    let work = channel.take().unwrap();
    publisher
        .update(|state| state.presented(8, Evidence::SubmittedToCompositor, Instant::now()))
        .unwrap();
    work.complete(Ok(image()));
    assert!(matches!(
        poll(capture.as_mut()),
        Poll::Ready(Err(Refusal::NotCurrent))
    ));
    assert!(channel.take().is_none());
}
#[test]
fn queued_capture_cancellation_releases_only_its_own_request() {
    let (publisher, channel, handle) = ready();
    let mut capture = Box::pin(publisher.capture());
    assert!(poll(capture.as_mut()).is_pending());
    drop(capture);
    assert!(channel.take().is_none());
    assert!(handle.request(stamp()).is_ok());
    assert_eq!(publisher.read().unwrap().phase, Phase::Streaming);
}

#[test]
fn capture_waiter_is_send_without_carrying_a_publication_guard() {
    fn send<T: Send>(_: T) {}
    let (publisher, _channel, _handle) = ready();
    send(publisher.capture());
}
