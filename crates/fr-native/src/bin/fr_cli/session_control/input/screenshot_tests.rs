use super::super::Candidate;
use super::*;
use crate::robot_input::Response;
use fr_client::robot::snapshot::Channel;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration,
        ViewportMappingGeneration,
    },
    input::InputView,
};
use fr_wire::{
    SourceObservation,
    presented::{Sample, Stamp as SourceStamp},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
fn stamp() -> Stamp {
    Stamp {
        frame: FrameId::from_raw(7),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn image() -> fr_media::worker::presentation_snapshot::Snapshot {
    use fr_media::worker::presentation_snapshot::Snapshot;
    Snapshot::decode(Snapshot::encode(stamp(), 2, 2, &[0, 0, 0, 255].repeat(4)).unwrap()).unwrap()
}
fn ready() -> (Shared, Channel) {
    let shared = Shared::new([1; 16], [2; 16]).unwrap();
    shared.locked().live = Some(Candidate {
        bounds: fr_core::input::InputBounds::new(
            fr_core::input::DesktopPoint { x: -800, y: 0 },
            800,
            600,
        )
        .unwrap(),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        sample: Sample {
            stamp: SourceStamp {
                frame: 7,
                captured_us: 10,
                observed_us: 10,
                source: SourceObservation::Captured,
            },
            age_upper_us: 0,
        },
        at: Instant::now(),
    });
    let (channel, handle) = Channel::new();
    channel.publish(Some(stamp()));
    shared
        .bind_snapshot(
            handle,
            Display {
                handle: 17,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: -800,
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
        )
        .unwrap();
    (shared, channel)
}
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
#[test]
fn image_returns_the_original_pin_and_new_status_invalidates_its_seal() {
    let (shared, channel) = ready();
    let mut capture = Box::pin(shared.capture());
    assert!(poll(capture.as_mut()).is_pending());
    let original_id = shared.locked().pinned.unwrap().0;
    channel.take().unwrap().complete(Ok(image()));
    let Poll::Ready(Ok((metadata, image))) = poll(capture.as_mut()) else {
        panic!("matching export");
    };
    let control = metadata.control.as_ref().unwrap();
    assert_eq!(control.observation_id, Some(original_id));
    assert_eq!(control.frame, Some(7));
    assert_eq!(shared.check_export(&metadata), Ok(()));
    let Response::Status { snapshot } = shared.status() else {
        panic!("status");
    };
    assert_eq!(snapshot.observation_id, original_id.checked_add(1));
    assert_eq!(shared.check_export(&metadata), Err(Refusal::NotCurrent));
    assert_eq!(
        shared.locked().pinned.unwrap().0,
        snapshot.observation_id.unwrap()
    );
    drop(image);
}
#[test]
fn status_racing_native_export_never_repins_or_returns_newer_authority() {
    let (shared, channel) = ready();
    let mut capture = Box::pin(shared.capture());
    assert!(poll(capture.as_mut()).is_pending());
    let work = channel.take().unwrap();
    let Response::Status { snapshot } = shared.status() else {
        panic!("status");
    };
    work.complete(Ok(image()));
    assert!(matches!(
        poll(capture.as_mut()),
        Poll::Ready(Err(Refusal::NotCurrent))
    ));
    assert_eq!(
        shared.locked().pinned.unwrap().0,
        snapshot.observation_id.unwrap()
    );
}
#[test]
fn recovery_and_retirement_refuse_the_original_pending_export() {
    for retired in [false, true] {
        let (shared, channel) = ready();
        let mut capture = Box::pin(shared.capture());
        assert!(poll(capture.as_mut()).is_pending());
        let work = channel.take().unwrap();
        if retired {
            shared.retire();
        } else {
            shared.locked().live.as_mut().unwrap().view.recovery = RecoveryGeneration::from_raw(1);
        }
        work.complete(Ok(image()));
        let expected = if retired {
            Refusal::Closed
        } else {
            Refusal::NotCurrent
        };
        assert!(matches!(poll(capture.as_mut()), Poll::Ready(Err(reason)) if reason == expected));
    }
}
#[test]
fn expired_source_and_pending_input_do_not_begin_an_export() {
    let (shared, channel) = ready();
    shared.locked().live.as_mut().unwrap().sample.age_upper_us = 1_000_000;
    let mut capture = Box::pin(shared.capture());
    assert!(matches!(
        poll(capture.as_mut()),
        Poll::Ready(Err(Refusal::NotReady))
    ));
    assert!(channel.take().is_none());
    let (shared, channel) = ready();
    let Response::Status { snapshot } = shared.status() else {
        panic!("status");
    };
    let request = crate::robot_input::Request {
        lease: crate::robot_input::id(snapshot.lease.as_deref().unwrap()).unwrap(),
        request_id: snapshot.next_request_id.unwrap(),
        observation_id: snapshot.observation_id.unwrap(),
        max_observation_age_ms: 1000,
        operation: crate::robot_input::Operation::KeyTap { usage: 4 },
    };
    assert_eq!(shared.locked().submit(request), Ok(None));
    let mut capture = Box::pin(shared.capture());
    assert!(matches!(
        poll(capture.as_mut()),
        Poll::Ready(Err(Refusal::Busy))
    ));
    assert!(channel.take().is_none());
    assert!(
        !shared.locked().stopped,
        "refusing a read cannot cancel an input action"
    );
}

#[test]
fn capture_waiter_is_send_without_carrying_a_shared_state_guard() {
    fn send<T: Send>(_: T) {}
    let (shared, _channel) = ready();
    send(shared.capture());
}
