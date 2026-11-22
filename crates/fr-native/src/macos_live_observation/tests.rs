use super::*;
use fr_core::ids::DisplayGeometryGeneration;
use std::time::Duration;

fn display() -> Display {
    Display {
        handle: 17,
        geometry: DisplayGeometryGeneration::INITIAL,
        x: -1920,
        y: 0,
        pixel_width: 1920,
        pixel_height: 1080,
        logical_width: 960,
        logical_height: 540,
        scale_numerator: 2,
        scale_denominator: 1,
        rotation: 0,
    }
}

fn owner() -> (Publisher, Rc<RefCell<Observation>>) {
    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let publisher =
        Publisher::new(move |event| event.apply(&mut sink.borrow_mut()).map_err(|_| ()));
    (publisher, state)
}

fn streaming(owner: &Publisher, attempt: u8, at: Instant) -> Attempt {
    owner.begin(attempt).unwrap();
    owner.authenticated(attempt).unwrap();
    let original = owner.original(attempt).unwrap();
    original
        .ready(display(), 0, Evidence::SubmittedToCompositor, at)
        .unwrap();
    original
}

#[test]
fn delayed_startup_keeps_the_original_receipt_age_including_duplicate_callbacks() {
    let (owner, state) = owner();
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    // The native bootstrap may wait for its network acknowledgement after the
    // receipt. Registering the already-open observer cannot give it a new age.
    let later = at + Duration::from_secs(7);
    let snapshot = state.borrow().snapshot(later).unwrap();
    assert_eq!(snapshot.frame.unwrap().receipt_age_ms, 7000);
    original
        .presented(0, Evidence::SubmittedToCompositor, later)
        .unwrap();
    let snapshot = state.borrow().snapshot(later).unwrap();
    assert_eq!(snapshot.frame.unwrap().receipt_age_ms, 7000);
}

#[test]
fn later_real_receipt_changes_the_frame_and_evidence_without_changing_geometry() {
    let (owner, state) = owner();
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    original
        .presented(9, Evidence::Decoded, at + Duration::from_secs(2))
        .unwrap();
    let snapshot = state
        .borrow()
        .snapshot(at + Duration::from_millis(2500))
        .unwrap();
    snapshot.validate().unwrap();
    let frame = snapshot.frame.unwrap();
    assert_eq!(frame.frame_serial, 9);
    assert_eq!(frame.evidence_level, Evidence::Decoded);
    assert_eq!(frame.receipt_age_ms, 500);
    assert_eq!(frame.origin_x, -1920);
    assert_eq!(frame.display_handle, format!("{:032x}", 17));
}

#[test]
fn approval_and_authentication_do_not_publish_a_frame() {
    let (owner, state) = owner();
    owner.begin(1).unwrap();
    owner.authenticated(1).unwrap();
    let original = owner.original(1).unwrap();
    original.approval().unwrap();
    original.approval().unwrap();
    let snapshot = state.borrow().snapshot(Instant::now()).unwrap();
    assert_eq!(snapshot.phase, Phase::AwaitingApproval);
    assert!(snapshot.frame.is_none());
}

#[test]
fn retirement_fences_receipts_and_old_callbacks_cannot_touch_a_replacement() {
    for phase in [Phase::Cleaning, Phase::Reconnecting, Phase::Stopped] {
        let (owner, state) = owner();
        let at = Instant::now();
        let old = streaming(&owner, 1, at);
        owner.retire(phase).unwrap();
        assert!(old.presented(99, Evidence::Decoded, at).is_err());
        assert!(old.approval().is_err());
        let retired = state.borrow().snapshot(at).unwrap();
        assert_eq!(retired.phase, phase);
        assert!(retired.frame.is_none());
        let _current = streaming(&owner, 2, at);
        let current = state.borrow().snapshot(at).unwrap();
        assert!(old.ready(display(), 99, Evidence::Decoded, at).is_err());
        assert!(old.presented(99, Evidence::Decoded, at).is_err());
        assert_eq!(state.borrow().snapshot(at).unwrap(), current);
    }
}

#[test]
fn failed_publication_is_terminal_and_never_retries_with_a_fresh_attempt() {
    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let owner = Publisher::new(move |event| {
        if matches!(event, Event::Presented { .. }) {
            return Err(());
        }
        event.apply(&mut sink.borrow_mut()).map_err(|_| ())
    });
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    assert!(original.presented(1, Evidence::Decoded, at).is_err());
    let snapshot = state.borrow().snapshot(at).unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
    assert!(original.presented(2, Evidence::Decoded, at).is_err());
    assert!(owner.begin(2).is_err());
}

#[test]
fn an_ignored_reentrant_failure_cannot_turn_the_outer_receipt_into_success() {
    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let retained = Rc::new(RefCell::new(None::<Attempt>));
    let reentrant = Rc::clone(&retained);
    let owner = Publisher::new(move |event| {
        if let Event::Presented { at, .. } = event {
            let original = reentrant.borrow();
            // Deliberately ignore the inner callback's refusal. The original
            // owner must still reject the outer callback's apparent success.
            let _ = original
                .as_ref()
                .unwrap()
                .presented(99, Evidence::Decoded, at);
        }
        event.apply(&mut sink.borrow_mut()).map_err(|_| ())
    });
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    *retained.borrow_mut() = Some(original.clone());
    assert!(original.presented(1, Evidence::Decoded, at).is_err());
    let snapshot = state.borrow().snapshot(at).unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
    assert!(owner.begin(2).is_err());
    // The failure remains terminal, but later successful metadata retirement
    // need not misreport the original native owners' confirmed cleanup.
    owner.retire(Phase::Cleaning).unwrap();
    assert!(owner.begin(2).is_err());
}

#[test]
fn dropping_the_owner_fences_retained_attempt_callbacks() {
    let (owner, state) = owner();
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    drop(owner);
    assert!(original.presented(1, Evidence::Decoded, at).is_err());
    let snapshot = state.borrow().snapshot(at).unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
}

#[test]
fn attempts_are_bounded_and_cannot_replace_a_live_or_reuse_a_retired_attempt() {
    let (owner, state) = owner();
    let at = Instant::now();
    for invalid in [0, 33, u8::MAX] {
        assert!(owner.begin(invalid).is_err());
    }
    let _original = streaming(&owner, 1, at);
    assert!(owner.begin(2).is_err(), "live owner was not retired");
    assert!(owner.authenticated(2).is_err());
    assert!(owner.original(2).is_err());
    assert_eq!(state.borrow().snapshot(at).unwrap().attempt, 1);
    owner.retire(Phase::Cleaning).unwrap();
    assert!(owner.begin(1).is_err());
    let _current = streaming(&owner, 2, at);
    assert_eq!(state.borrow().snapshot(at).unwrap().attempt, 2);
}

#[test]
fn backwards_receipt_clock_stops_publication_without_fabricated_age() {
    let (owner, state) = owner();
    let at = Instant::now();
    let original = streaming(&owner, 1, at + Duration::from_secs(1));
    assert!(original.presented(1, Evidence::Decoded, at).is_err());
    let snapshot = state.borrow().snapshot(at).unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
}

#[test]
fn original_snapshot_attachment_is_once_only_and_does_not_publish_a_frame() {
    use fr_client::robot::snapshot::Channel;

    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let attached = Rc::new(RefCell::new(Vec::new()));
    let bindings = Rc::clone(&attached);
    let owner = Publisher::new(move |event| match event {
        Event::Snapshot(handle) => {
            bindings.borrow_mut().push(handle);
            Ok(())
        }
        event => event.apply(&mut sink.borrow_mut()).map_err(|_| ()),
    });
    owner.begin(1).unwrap();
    owner.authenticated(1).unwrap();
    let original = owner.original(1).unwrap();
    let (channel, handle) = Channel::new();
    original.bind_snapshot(handle.clone()).unwrap();
    assert!(original.bind_snapshot(handle).is_err());
    assert_eq!(attached.borrow().len(), 1);
    assert!(attached.borrow()[0].current_stamp().is_none());
    let snapshot = state.borrow().snapshot(Instant::now()).unwrap();
    assert_eq!(snapshot.phase, Phase::Authenticated);
    assert!(snapshot.frame.is_none());

    let at = Instant::now();
    original
        .ready(display(), 0, Evidence::SubmittedToCompositor, at)
        .unwrap();
    let stamp = fr_media::worker::presentation_snapshot::Stamp {
        frame: fr_media::access_unit::FrameId::from_raw(0),
        configuration: fr_core::ids::CodecConfigurationGeneration::INITIAL,
        recovery: fr_core::ids::RecoveryGeneration::INITIAL,
    };
    channel.publish(Some(stamp));
    assert_eq!(attached.borrow()[0].current_stamp(), Some(stamp));
    drop(channel);
    assert!(attached.borrow()[0].current_stamp().is_none());
}

#[test]
fn a_retired_snapshot_callback_cannot_install_into_the_replacement() {
    use fr_client::robot::snapshot::Channel;

    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let attached = Rc::new(Cell::new(0));
    let bindings = Rc::clone(&attached);
    let owner = Publisher::new(move |event| match event {
        Event::Snapshot(_) => {
            bindings.set(bindings.get() + 1);
            Ok(())
        }
        event => event.apply(&mut sink.borrow_mut()).map_err(|_| ()),
    });
    let at = Instant::now();
    let old = streaming(&owner, 1, at);
    owner.retire(Phase::Cleaning).unwrap();
    let current = streaming(&owner, 2, at);
    let (_channel, handle) = Channel::new();
    assert!(old.bind_snapshot(handle.clone()).is_err());
    assert_eq!(attached.get(), 0);
    current.bind_snapshot(handle.clone()).unwrap();
    assert_eq!(attached.get(), 1);
    drop(owner);
    assert!(current.bind_snapshot(handle).is_err());
    assert_eq!(attached.get(), 1);
}

#[test]
fn failed_snapshot_binding_retires_original_metadata_and_cannot_retry() {
    use fr_client::robot::snapshot::Channel;

    let state = Rc::new(RefCell::new(Observation::default()));
    let sink = Rc::clone(&state);
    let owner = Publisher::new(move |event| match event {
        Event::Snapshot(_) => Err(()),
        event => event.apply(&mut sink.borrow_mut()).map_err(|_| ()),
    });
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    let (_channel, handle) = Channel::new();
    assert!(original.bind_snapshot(handle.clone()).is_err());
    let snapshot = state.borrow().snapshot(at).unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
    assert!(original.bind_snapshot(handle).is_err());
    assert!(owner.begin(2).is_err());
}

#[test]
fn a_metadata_only_sink_cannot_claim_it_bound_an_export_handle() {
    let (owner, state) = owner();
    let at = Instant::now();
    let original = streaming(&owner, 1, at);
    let (_channel, handle) = fr_client::robot::snapshot::Channel::new();
    assert!(original.bind_snapshot(handle).is_err());
    assert_eq!(state.borrow().snapshot(at).unwrap().phase, Phase::Stopped);
}
