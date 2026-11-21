use super::*;
use fr_client::robot::{live::Evidence, snapshot::Channel};
use fr_core::ids::DisplayGeometryGeneration;
use fr_wire::display::Display;

fn ready() -> Publisher {
    let publisher = Publisher::default();
    publisher.begin(1).unwrap();
    publisher
        .update(|observation| {
            observation.authenticated()?;
            observation.ready(
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
    publisher
}

#[test]
fn retirement_is_readable_without_waiting_for_either_contended_slot() {
    for phase in [Phase::Cleaning, Phase::Reconnecting, Phase::Stopped] {
        let publisher = ready();
        let (_channel, source) = Channel::new();
        publisher.bind_snapshot(source).unwrap();
        let observation = publisher.0.lock().unwrap();
        let handle = publisher.1.lock().unwrap();

        publisher.retire(phase);
        let snapshot = publisher.read().unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.attempt, 1);
        assert_eq!(snapshot.phase, phase);
        assert!(snapshot.frame.is_none());
        // Prove this exercises the atomic fence, not successful mutex cleanup.
        assert!(
            observation
                .snapshot(Instant::now())
                .unwrap()
                .frame
                .is_some()
        );
        assert!(handle.is_some());

        drop(observation);
        drop(handle);
        assert_eq!(publisher.read().unwrap(), snapshot);
        assert!(publisher.update(|_| Ok(())).is_err());
    }
}

#[test]
fn a_late_positive_update_cannot_undo_a_retirement_during_its_callback() {
    let publisher = ready();
    let original = publisher.clone();
    assert!(
        publisher
            .update(|observation| {
                original.retire(Phase::Cleaning);
                observation.presented(8, Evidence::Decoded, Instant::now())
            })
            .is_err()
    );
    let snapshot = publisher.read().unwrap();
    assert_eq!(snapshot.phase, Phase::Cleaning);
    assert!(snapshot.frame.is_none());
}

#[test]
fn a_retired_slot_cannot_be_reopened_by_an_ordinary_update() {
    let publisher = ready();
    publisher.retire(Phase::Reconnecting);
    let mut called = false;
    assert!(
        publisher
            .update(|observation| {
                called = true;
                observation.begin(2)
            })
            .is_err()
    );
    assert!(!called);
    publisher.begin(2).unwrap();
    let snapshot = publisher.read().unwrap();
    assert_eq!(snapshot.attempt, 2);
    assert_eq!(snapshot.phase, Phase::Connecting);
    assert!(snapshot.frame.is_none());
}

#[test]
fn an_explicit_begin_is_monotonic_and_never_replaces_a_live_attempt() {
    let publisher = ready();
    for attempt in [0, 1, 2, 33, u8::MAX] {
        assert!(publisher.begin(attempt).is_err());
    }
    assert_eq!(publisher.read().unwrap().attempt, 1);
    publisher.retire(Phase::Cleaning);
    assert!(publisher.begin(1).is_err());
    publisher.begin(2).unwrap();
    assert!(publisher.begin(2).is_err());
    assert_eq!(publisher.read().unwrap().attempt, 2);
}

#[test]
fn a_failed_update_hides_its_previous_frame_even_if_the_action_does_not_clear_it() {
    let publisher = ready();
    assert!(
        publisher
            .update(|_| Err(fr_client::robot::live::Error::Order))
            .is_err()
    );
    let snapshot = publisher.read().unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
}

#[test]
fn a_busy_writer_fences_the_original_frame_without_blocking_on_the_reader() {
    let publisher = ready();
    let held = publisher.0.lock().unwrap();
    let mut called = false;
    assert!(
        publisher
            .update(|_| {
                called = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!called);
    let snapshot = publisher.read().unwrap();
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
    drop(held);
    assert_eq!(publisher.read().unwrap(), snapshot);
}

#[test]
fn the_next_attempt_drops_a_source_left_stored_by_contended_retirement() {
    let publisher = ready();
    let (_channel, old_source) = Channel::new();
    publisher.bind_snapshot(old_source).unwrap();
    let held = publisher.1.lock().unwrap();
    publisher.retire(Phase::Cleaning);
    assert!(held.is_some());
    assert!(
        publisher.begin(2).is_err(),
        "a held handle slot must not be waited on"
    );
    assert_eq!(publisher.read().unwrap().attempt, 1);
    drop(held);
    publisher.begin(2).unwrap();
    assert!(publisher.1.lock().unwrap().is_none());
    assert_eq!(publisher.read().unwrap().attempt, 2);
}

#[test]
fn delayed_cleanup_and_failed_old_writers_cannot_clear_a_replacement() {
    let publisher = ready();
    let original = publisher.gate();
    publisher.retire(Phase::Cleaning);
    let retired = publisher.gate();
    publisher.begin(2).unwrap();
    let (_channel, new_source) = Channel::new();
    publisher.bind_snapshot(new_source).unwrap();
    let current = publisher.read().unwrap();
    publisher.clear_retired(retired);
    publisher.retire_if_current(original, Phase::Stopped);
    assert_eq!(publisher.read().unwrap(), current);
    assert!(publisher.1.lock().unwrap().is_some());
}

#[test]
fn a_retired_slot_refuses_a_late_snapshot_handle() {
    let publisher = ready();
    publisher.retire(Phase::Stopped);
    let (_channel, source) = Channel::new();
    assert!(publisher.bind_snapshot(source).is_err());
    assert!(publisher.1.lock().unwrap().is_none());
}

#[test]
fn repeated_retirement_advances_the_gate_and_counter_exhaustion_stays_closed() {
    let publisher = ready();
    publisher.retire(Phase::Cleaning);
    let first = publisher.gate();
    publisher.retire(Phase::Cleaning);
    assert_ne!(publisher.gate().0, first.0);

    // Exercise the real terminal counter boundary, without allowing wraparound
    // to recreate a token that could authorize an old reader or writer.
    publisher
        .2
        .store(!(REVISION - 1) | (1 << 8), Ordering::SeqCst);
    publisher.retire(Phase::Cleaning);
    let exhausted = publisher.gate();
    assert_eq!(exhausted.state(), EXHAUSTED);
    assert!(publisher.begin(2).is_err());
    assert!(publisher.update(|_| Ok(())).is_err());
    publisher.retire(Phase::Reconnecting);
    assert_eq!(publisher.gate().0, exhausted.0);
    let snapshot = publisher.read().unwrap();
    snapshot.validate().unwrap();
    assert_eq!(snapshot.attempt, 1);
    assert_eq!(snapshot.phase, Phase::Stopped);
    assert!(snapshot.frame.is_none());
}

#[test]
fn retirement_before_an_attempt_never_fabricates_connecting_metadata() {
    for phase in [
        Phase::Cleaning,
        Phase::Reconnecting,
        Phase::Streaming,
        Phase::Stopped,
    ] {
        let publisher = Publisher::default();
        publisher.retire(phase);
        let snapshot = publisher.read().unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.attempt, 0);
        assert_eq!(snapshot.phase, Phase::Stopped);
        assert!(snapshot.frame.is_none());
    }
}
