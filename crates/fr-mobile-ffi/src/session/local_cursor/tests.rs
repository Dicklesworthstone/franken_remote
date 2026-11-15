use super::*;

fn bound() -> (Hub, Arc<Owner>, Arc<AtomicBool>) {
    let hub = Hub::default();
    hub.begin_attempt(1).unwrap();
    hub.configure(1, 7).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let owner = hub.bind(1, (1920, 1080), Arc::clone(&stop)).unwrap();
    (hub, owner, stop)
}

#[test]
fn hostile_shape_geometry_and_length_are_refused_before_copying() {
    for (width, height, x, y, rgba) in [
        (0, 1, 0, 0, &[][..]),
        (1, 0, 0, 0, &[][..]),
        (257, 1, 0, 0, &[][..]),
        (1, 257, 0, 0, &[][..]),
        (2, 2, 2, 0, &[0; 16][..]),
        (2, 2, 0, 2, &[0; 16][..]),
        (2, 2, 0, 0, &[0; 15][..]),
        (2, 2, 0, 0, &[0; 17][..]),
    ] {
        assert!(matches!(
            Shape::new(width, height, x, y, rgba),
            Err(Error::Invalid)
        ));
    }
    let shape = Shape::new(2, 1, 1, 0, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!(shape.geometry(), (2, 1, 1, 0));
    assert_eq!(shape.rgba(), &[1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn one_pending_image_replaces_older_requests_without_acknowledging_them() {
    let (_, owner, _) = bound();
    let first = owner.request(|| Ok(Image::Default)).unwrap();
    let old = owner.take().unwrap().unwrap();
    let second = owner.request(|| Ok(Image::Blank)).unwrap();
    let third = owner.request(|| Ok(Image::Default)).unwrap();
    assert!(first < second && second < third);
    assert!(!owner.acknowledge(&old.stamp).unwrap());
    assert_eq!(owner.applied(), 0);
    let latest = owner.take().unwrap().unwrap();
    assert_eq!(latest.stamp.generation, third);
    assert!(matches!(latest.image, Image::Default));
    assert!(owner.take().unwrap().is_none());
    assert!(owner.acknowledge(&latest.stamp).unwrap());
    assert_eq!(owner.applied(), third);
}

#[test]
fn reset_during_the_owned_copy_cannot_relabel_a_request() {
    let (hub, owner, _) = bound();
    let original = owner.scope().unwrap();
    let result = owner.request(|| {
        hub.configure(1, 7).unwrap();
        Ok(Image::Blank)
    });
    assert_eq!(result, Err(Error::Busy));
    assert!(owner.take().unwrap().is_none());
    assert_eq!(owner.applied(), 0);
    assert!(owner.scope().unwrap().configuration > original.configuration);
}

#[test]
fn reset_with_no_new_video_still_fences_native_completion() {
    let (hub, owner, _) = bound();
    owner.request(|| Ok(Image::Blank)).unwrap();
    let original = owner.take().unwrap().unwrap();
    hub.configure(1, original.stamp.scope().frame_after)
        .unwrap();
    assert!(!owner.current(&original.stamp).unwrap());
    assert!(!owner.acknowledge(&original.stamp).unwrap());
    assert_eq!(owner.applied(), 0);
    owner.request(|| Ok(Image::Default)).unwrap();
    let replacement = owner.take().unwrap().unwrap();
    assert_ne!(replacement.stamp.scope(), original.stamp.scope());
    assert!(owner.acknowledge(&replacement.stamp).unwrap());
}

#[test]
fn original_source_stop_is_rechecked_after_preparation() {
    let (_, owner, stop) = bound();
    assert_eq!(
        owner.request(|| {
            stop.store(true, Ordering::SeqCst);
            Ok(Image::Blank)
        }),
        Err(Error::Stopped)
    );
    assert!(owner.stopped());
    assert!(!owner.completed());
}

#[test]
fn a_new_attempt_waits_for_actual_native_completion_not_stop() {
    let (hub, owner, stop) = bound();
    stop.store(true, Ordering::SeqCst);
    owner.stop();
    assert_eq!(hub.begin_attempt(2), Err(Error::Busy));
    owner.complete();
    hub.begin_attempt(2).unwrap();
    hub.configure(2, 9).unwrap();
    let fresh_stop = Arc::new(AtomicBool::new(false));
    let replacement = hub.bind(2, (1280, 720), Arc::clone(&fresh_stop)).unwrap();
    owner.stop();
    owner.complete();
    assert!(!replacement.stopped());
    assert!(!replacement.completed());
    assert!(!fresh_stop.load(Ordering::SeqCst));
    assert!(!Arc::ptr_eq(&owner, &replacement));
    assert_eq!(replacement.display(), (1280, 720));
    assert_eq!(hub.configure(1, 10), Err(Error::Invalid));
    assert_eq!(replacement.scope().unwrap().attempt, 2);
}

#[test]
fn request_exhaustion_preserves_the_last_admitted_slot() {
    let (_, owner, _) = bound();
    owner.request(|| Ok(Image::Blank)).unwrap();
    {
        let mut queue = owner.queue.lock().unwrap();
        queue.generation = u64::MAX;
    }
    assert_eq!(owner.request(|| Ok(Image::Default)), Err(Error::Stopped));
    let queue = owner.queue.lock().unwrap();
    assert_eq!(queue.generation, u64::MAX);
    assert!(matches!(
        queue.pending.as_ref().map(|request| &request.image),
        Some(Image::Blank)
    ));
}

#[test]
fn contended_exchange_is_nonblocking_and_carries_no_false_acknowledgement() {
    let (_, owner, _) = bound();
    owner.request(|| Ok(Image::Blank)).unwrap();
    let request = owner.take().unwrap().unwrap();
    let _held = owner.queue.lock().unwrap();
    assert_eq!(owner.request(|| Ok(Image::Default)), Err(Error::Busy));
    assert!(matches!(owner.take(), Err(Error::Busy)));
    assert_eq!(owner.acknowledge(&request.stamp), Err(Error::Busy));
    assert_eq!(owner.applied(), 0);
    owner.observe(Pointer::Outside);
    assert_eq!(owner.pointer(), Pointer::Outside);
    owner.stop();
    assert!(owner.stopped());
    assert!(!owner.completed());
}

#[test]
fn exhausted_configuration_cannot_partially_invalidate_an_owner() {
    let (hub, owner, _) = bound();
    owner.request(|| Ok(Image::Blank)).unwrap();
    let request = owner.take().unwrap().unwrap();
    hub.binding.lock().unwrap().scope.configuration = u64::MAX;
    assert_eq!(hub.configure(1, 8), Err(Error::Stopped));
    assert!(owner.current(&request.stamp).unwrap());
    assert_eq!(owner.scope().unwrap().frame_after, 7);
}

#[test]
fn matching_numeric_generations_cannot_repair_a_different_original_owner() {
    let (_, original, _) = bound();
    let (_, replacement, _) = bound();
    original.request(|| Ok(Image::Blank)).unwrap();
    replacement.request(|| Ok(Image::Default)).unwrap();
    let old = original.take().unwrap().unwrap();
    let new = replacement.take().unwrap().unwrap();
    assert_eq!(old.stamp.scope, new.stamp.scope);
    assert_eq!(old.stamp.generation, new.stamp.generation);
    assert!(!replacement.current(&old.stamp).unwrap());
    assert!(!replacement.acknowledge(&old.stamp).unwrap());
    assert_eq!(replacement.applied(), 0);
    assert!(replacement.acknowledge(&new.stamp).unwrap());
}
