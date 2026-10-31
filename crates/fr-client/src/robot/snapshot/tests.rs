use super::*;
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_media::access_unit::FrameId;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Wake,
};

struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn waker() -> (Arc<WakeCount>, Waker) {
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    (count.clone(), Waker::from(count))
}
fn stamp() -> Stamp {
    Stamp {
        frame: FrameId::from_raw(7),
        configuration: CodecConfigurationGeneration::from_raw(3),
        recovery: RecoveryGeneration::from_raw(2),
    }
}
fn snapshot(stamp: Stamp) -> Snapshot {
    Snapshot::decode(Snapshot::encode(stamp, 2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap()).unwrap()
}
fn setup() -> (Channel, Handle, Instant) {
    let (channel, handle) = Channel::new();
    channel.publish(Some(stamp()));
    (channel, handle, Instant::now())
}
fn read_image(request: &mut Request, at: Instant) -> Image {
    let (_, waker) = waker();
    match request.poll_at(&mut Context::from_waker(&waker), at) {
        Poll::Ready(Ok(image)) => image,
        _ => panic!("expected the completed original image"),
    }
}
fn error(request: &mut Request, at: Instant, expected: Error) {
    let (_, waker) = waker();
    assert!(
        matches!(request.poll_at(&mut Context::from_waker(&waker), at), Poll::Ready(Err(actual)) if actual == expected)
    );
}

#[test]
fn one_reservation_covers_native_work_ready_pixels_and_local_transfer() {
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    let (count, waker) = waker();
    assert!(
        request
            .poll_at(&mut Context::from_waker(&waker), at)
            .is_pending()
    );
    let work = channel.take_at(at).unwrap();
    assert_eq!(work.stamp(), stamp());
    assert_eq!(work.deadline(), at + REQUEST_TIMEOUT);
    assert_eq!(work.deadline(), request.deadline());
    assert!(channel.take_at(at).is_none());
    assert!(matches!(
        handle.clone().request_at(stamp(), at),
        Err(Error::Busy)
    ));
    let pixels = snapshot(stamp());
    let pointer = pixels.pixels().as_ptr();
    work.complete_at(Ok(pixels), at);
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    assert!(matches!(handle.request_at(stamp(), at), Err(Error::Busy)));
    let image = read_image(&mut request, at);
    assert_eq!(image.snapshot().pixels().as_ptr(), pointer);
    assert_eq!(image.snapshot().stamp(), stamp());
    assert_eq!(image.deadline(), at + REQUEST_TIMEOUT);
    assert!(image.check_at(at).is_ok());
    drop(request);
    assert!(matches!(handle.request_at(stamp(), at), Err(Error::Busy)));
    drop(image);
    assert!(handle.request_at(stamp(), at).is_ok());
}

#[test]
fn missing_and_foreign_presentations_never_reserve_work() {
    let (channel, handle) = Channel::new();
    assert_eq!(handle.current_stamp(), None);
    assert!(matches!(handle.request(stamp()), Err(Error::Unavailable)));
    channel.publish(Some(stamp()));
    assert_eq!(handle.current_stamp(), Some(stamp()));
    for different in [
        Stamp {
            frame: FrameId::from_raw(8),
            ..stamp()
        },
        Stamp {
            configuration: CodecConfigurationGeneration::from_raw(4),
            ..stamp()
        },
        Stamp {
            recovery: RecoveryGeneration::from_raw(3),
            ..stamp()
        },
    ] {
        assert!(matches!(handle.request(different), Err(Error::NotCurrent)));
        assert!(channel.take().is_none());
    }
}

#[test]
fn unpolled_or_ready_reader_cancellation_releases_only_its_reservation() {
    let (channel, handle, at) = setup();
    drop(handle.request_at(stamp(), at).unwrap());
    assert!(channel.take_at(at).is_none());
    let request = handle.request_at(stamp(), at).unwrap();
    channel
        .take_at(at)
        .unwrap()
        .complete_at(Ok(snapshot(stamp())), at);
    drop(request);
    assert!(handle.request_at(stamp(), at).is_ok());
    assert_eq!(handle.current_stamp(), Some(stamp()));
}

#[test]
fn cancelled_running_reader_stays_busy_until_the_original_work_drains() {
    let (channel, handle, at) = setup();
    let request = handle.request_at(stamp(), at).unwrap();
    let work = channel.take_at(at).unwrap();
    drop(request);
    assert!(matches!(handle.request_at(stamp(), at), Err(Error::Busy)));
    assert!(channel.take_at(at).is_none());
    work.complete_at(Ok(snapshot(stamp())), at);
    let mut next = handle.request_at(stamp(), at).unwrap();
    drop(channel.take_at(at).unwrap());
    error(&mut next, at, Error::Unavailable);
    assert!(handle.request_at(stamp(), at).is_ok());
}

#[test]
fn abandoned_work_wakes_the_waiter_without_closing_the_viewer() {
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    let (count, waker) = waker();
    assert!(
        request
            .poll_at(&mut Context::from_waker(&waker), at)
            .is_pending()
    );
    drop(channel.take_at(at).unwrap());
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    error(&mut request, at, Error::Unavailable);
    assert_eq!(handle.current_stamp(), Some(stamp()));
    assert!(handle.request_at(stamp(), at).is_ok());
}

#[test]
fn every_wrong_completion_identity_is_refused_before_pixels_are_delivered() {
    let (channel, handle, at) = setup();
    for different in [
        Stamp {
            frame: FrameId::from_raw(8),
            ..stamp()
        },
        Stamp {
            configuration: CodecConfigurationGeneration::from_raw(4),
            ..stamp()
        },
        Stamp {
            recovery: RecoveryGeneration::from_raw(3),
            ..stamp()
        },
    ] {
        let mut request = handle.request_at(stamp(), at).unwrap();
        channel
            .take_at(at)
            .unwrap()
            .complete_at(Ok(snapshot(different)), at);
        error(&mut request, at, Error::NotCurrent);
    }
}

#[test]
fn publication_change_refuses_pending_and_running_requests_without_revival() {
    let (channel, handle, at) = setup();
    for running in [false, true] {
        channel.publish(Some(stamp()));
        let mut request = handle.request_at(stamp(), at).unwrap();
        let work = running.then(|| channel.take_at(at).unwrap());
        let (count, waker) = waker();
        assert!(
            request
                .poll_at(&mut Context::from_waker(&waker), at)
                .is_pending()
        );
        channel.publish(None);
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        channel.publish(Some(stamp()));
        error(&mut request, at, Error::NotCurrent);
        if let Some(work) = work {
            assert!(matches!(handle.request_at(stamp(), at), Err(Error::Busy)));
            work.complete_at(Ok(snapshot(stamp())), at);
        }
        assert!(handle.request_at(stamp(), at).is_ok());
    }
}

#[test]
fn newer_frame_before_completion_or_collection_refuses_the_old_request() {
    let (channel, handle, at) = setup();
    for completed in [false, true] {
        channel.publish(Some(stamp()));
        let mut request = handle.request_at(stamp(), at).unwrap();
        let work = channel.take_at(at).unwrap();
        if completed {
            work.complete_at(Ok(snapshot(stamp())), at);
            channel.publish(Some(Stamp {
                frame: FrameId::from_raw(8),
                ..stamp()
            }));
        } else {
            channel.publish(Some(Stamp {
                frame: FrameId::from_raw(8),
                ..stamp()
            }));
            work.complete_at(Ok(snapshot(stamp())), at);
        }
        error(&mut request, at, Error::NotCurrent);
    }
}

#[test]
fn queued_native_and_uncollected_expiry_share_the_original_inclusive_deadline() {
    let (channel, handle, at) = setup();
    for stage in 0..3 {
        let mut request = handle.request_at(stamp(), at).unwrap();
        let until = request.deadline();
        let (count, waker) = waker();
        assert!(
            request
                .poll_at(&mut Context::from_waker(&waker), at)
                .is_pending()
        );
        if stage == 0 {
            assert!(channel.take_at(until).is_none());
        } else {
            let work = channel.take_at(at).unwrap();
            assert_eq!(work.deadline(), until);
            work.complete_at(Ok(snapshot(stamp())), if stage == 1 { until } else { at });
        }
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        error(&mut request, until, Error::Expired);
    }
}

#[test]
fn native_expiry_wakes_and_refuses_reader_but_never_overlaps_native_work() {
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    let until = request.deadline();
    let work = channel.take_at(at).unwrap();
    let (count, waker) = waker();
    assert!(
        request
            .poll_at(&mut Context::from_waker(&waker), at)
            .is_pending()
    );
    channel.publish(Some(stamp()));
    assert_eq!(work.deadline(), until);
    assert!(channel.take_at(until).is_none());
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    error(&mut request, until, Error::Expired);
    assert!(matches!(
        handle.request_at(stamp(), until),
        Err(Error::Busy)
    ));
    work.complete_at(Ok(snapshot(stamp())), until);
    assert!(handle.request_at(stamp(), until).is_ok());
}

#[test]
fn image_remains_historical_across_frames_but_recovery_and_expiry_are_terminal() {
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    channel
        .take_at(at)
        .unwrap()
        .complete_at(Ok(snapshot(stamp())), at);
    let image = read_image(&mut request, at);
    channel.publish(Some(Stamp {
        frame: FrameId::from_raw(80),
        ..stamp()
    }));
    assert!(image.check_at(at).is_ok());
    assert_eq!(image.check_at(image.deadline()), Err(Error::Expired));
    channel.publish(None);
    assert_eq!(image.check_at(at), Err(Error::NotCurrent));
    channel.publish(Some(stamp()));
    assert_eq!(image.check_at(at), Err(Error::NotCurrent));
    assert!(matches!(handle.request_at(stamp(), at), Err(Error::Busy)));
    drop(image);
    for different in [
        Stamp {
            configuration: CodecConfigurationGeneration::from_raw(4),
            ..stamp()
        },
        Stamp {
            recovery: RecoveryGeneration::from_raw(3),
            ..stamp()
        },
    ] {
        channel.publish(Some(stamp()));
        let mut request = handle.request_at(stamp(), at).unwrap();
        channel
            .take_at(at)
            .unwrap()
            .complete_at(Ok(snapshot(stamp())), at);
        let image = read_image(&mut request, at);
        channel.publish(Some(different));
        assert_eq!(image.check_at(at), Err(Error::NotCurrent));
    }
}

#[test]
fn close_at_every_stage_wakes_and_never_revives_a_request_or_image() {
    for stage in 0..4 {
        let (channel, handle, at) = setup();
        let mut request = handle.request_at(stamp(), at).unwrap();
        let (count, waker) = waker();
        assert!(
            request
                .poll_at(&mut Context::from_waker(&waker), at)
                .is_pending()
        );
        let work = (stage > 0).then(|| channel.take_at(at).unwrap());
        let mut running = None;
        if stage > 1 {
            work.unwrap().complete_at(Ok(snapshot(stamp())), at);
        } else {
            running = work;
        }
        let image = (stage == 3).then(|| read_image(&mut request, at));
        drop(channel);
        assert_eq!(handle.current_stamp(), None);
        assert!(matches!(handle.request_at(stamp(), at), Err(Error::Closed)));
        if let Some(work) = running {
            work.complete_at(Ok(snapshot(stamp())), at);
        }
        if let Some(image) = image {
            assert_eq!(image.check_at(at), Err(Error::Closed));
        } else {
            error(&mut request, at, Error::Closed);
        }
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn only_the_latest_waiter_waker_is_retained_and_all_cross_thread_types_are_send() {
    fn send<T: Send>() {}
    send::<Handle>();
    send::<Request>();
    send::<Work>();
    send::<Image>();
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    let (first, first_waker) = waker();
    let (second, second_waker) = waker();
    assert!(
        request
            .poll_at(&mut Context::from_waker(&first_waker), at)
            .is_pending()
    );
    for _ in 0..8 {
        assert!(
            request
                .poll_at(&mut Context::from_waker(&second_waker), at)
                .is_pending()
        );
    }
    channel
        .take_at(at)
        .unwrap()
        .complete_at(Err(Error::Unavailable), at);
    assert_eq!(first.0.load(Ordering::Relaxed), 0);
    assert_eq!(second.0.load(Ordering::Relaxed), 1);
    error(&mut request, at, Error::Unavailable);
}

#[test]
fn poisoned_state_fences_both_endpoints_and_wakes_the_original_waiter() {
    let (channel, handle, at) = setup();
    let mut request = handle.request_at(stamp(), at).unwrap();
    let (count, waker) = waker();
    assert!(
        request
            .poll_at(&mut Context::from_waker(&waker), at)
            .is_pending()
    );
    let shared = channel.shared.clone();
    assert!(
        std::thread::spawn(move || {
            let _locked = shared.0.lock().unwrap();
            panic!("poison the metadata lock");
        })
        .join()
        .is_err()
    );
    assert_eq!(handle.current_stamp(), None);
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    error(&mut request, at, Error::Closed);
    assert!(matches!(handle.request_at(stamp(), at), Err(Error::Closed)));
}

#[test]
fn original_viewer_stop_fences_queued_running_ready_and_delivered_requests() {
    use std::sync::atomic::AtomicBool;
    for stage in 0..4 {
        let live = Arc::new(AtomicBool::new(true));
        let original = live.clone();
        let (channel, handle) = Channel::with_liveness(move || original.load(Ordering::Acquire));
        channel.publish(Some(stamp()));
        let at = Instant::now();
        let mut request = handle.request_at(stamp(), at).unwrap();
        let (count, waker) = waker();
        assert!(
            request
                .poll_at(&mut Context::from_waker(&waker), at)
                .is_pending()
        );
        let work = (stage > 0).then(|| channel.take_at(at).unwrap());
        let mut running = None;
        if stage > 1 {
            work.unwrap().complete_at(Ok(snapshot(stamp())), at);
        } else {
            running = work;
        }
        let image = (stage == 3).then(|| read_image(&mut request, at));

        // No Channel::close/publish or viewer-loop turn occurs before these
        // local reader checks: the original cancellation is sufficient.
        live.store(false, Ordering::Release);
        if let Some(image) = &image {
            assert_eq!(image.check_at(at), Err(Error::Closed));
        } else if stage == 0 {
            assert_eq!(handle.current_stamp(), None);
        } else {
            assert!(matches!(handle.request_at(stamp(), at), Err(Error::Closed)));
        }
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        assert_eq!(handle.current_stamp(), None);
        assert!(channel.take_at(at).is_none());
        // A subsequently true predicate or late native completion cannot revive
        // the original owner, nor can a new caller take its old reservation.
        live.store(true, Ordering::Release);
        channel.publish(Some(stamp()));
        if let Some(work) = running {
            work.complete_at(Ok(snapshot(stamp())), at);
        }
        if let Some(image) = image {
            assert_eq!(image.check_at(at), Err(Error::Closed));
        } else {
            error(&mut request, at, Error::Closed);
        }
        assert_eq!(handle.current_stamp(), None);
        assert!(matches!(handle.request_at(stamp(), at), Err(Error::Closed)));
    }
}

#[test]
fn liveness_predicate_is_never_called_under_the_mailbox_mutex() {
    let owner = Arc::new(Mutex::new(None::<std::sync::Weak<Shared>>));
    let observed = owner.clone();
    let (channel, handle) = Channel::with_liveness(move || {
        // A nonblocking probe of the private lock, not mailbox API re-entry.
        if let Some(shared) = observed
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            assert!(shared.0.try_lock().is_ok());
        }
        true
    });
    *owner.lock().unwrap() = Some(Arc::downgrade(&channel.shared));
    channel.publish(Some(stamp()));
    let at = Instant::now();
    let mut request = handle.request_at(stamp(), at).unwrap();
    channel
        .take_at(at)
        .unwrap()
        .complete_at(Ok(snapshot(stamp())), at);
    let image = read_image(&mut request, at);
    assert!(image.check_at(at).is_ok());
    drop(image);
}
