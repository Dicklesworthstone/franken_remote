//! Real X11 query/producer tests plus explicit lost-event and clock fixtures.
use super::super::held::{self, Snapshot};
use super::*;
use fr_core::held_state::HeldState;

fn snapshot_events(probe: &Probe) -> Vec<(HeldState, ClientInstant)> {
    probe
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(event, time)| {
            if let Event::HeldState(state) = event {
                Some((*state, *time))
            } else {
                None
            }
        })
        .collect()
}
#[test]
fn native_held_sampling_includes_back_forward_and_sleeps_when_nothing_is_held() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    thread::sleep(Duration::from_millis(270));
    assert_eq!(snapshot_events(&probe), []);
    for command in ["motion 40 50", "key 50 1", "button 8 1", "button 9 1"] {
        peer.command(command);
    }
    wait(|| snapshot_events(&probe).len() >= 3 || task.is_finished());
    let samples = snapshot_events(&probe);
    // Always clean up XTest state, including when a regression ended capture.
    for command in ["key 50 0", "button 8 0", "button 9 0"] {
        peer.command(command);
    }
    assert!(
        !task.is_finished(),
        "native sampling failed: {:?}",
        task.join().unwrap()
    );
    assert!(samples.len() >= 3);
    let latest = samples.last().unwrap().0;
    assert!(latest.key(PhysicalKey::new(225).unwrap())); // XKB LFSH
    assert!(latest.button(PointerButton::Back));
    assert!(latest.button(PointerButton::Forward));
    assert!(
        samples
            .windows(2)
            .all(|v| v[1].1.0 - v[0].1.0 >= held::INTERVAL_US)
    );
    wait(|| {
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| {
                matches!(
                    event,
                    Event::Key {
                        transition: KeyTransition::Release,
                        ..
                    } | Event::Positioned {
                        action: PositionedAction::Button { pressed: false, .. },
                        ..
                    }
                )
            })
            .count()
            == 3
            || task.is_finished()
    });
    let count = snapshot_events(&probe).len();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(snapshot_events(&probe).len(), count);
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    let events = probe.events.lock().unwrap();
    assert!(events.windows(2).all(|v| v[0].1 <= v[1].1));
    assert_eq!(
        events
            .iter()
            .filter(|(e, _)| matches!(
                e,
                Event::Key {
                    transition: KeyTransition::Press,
                    ..
                }
            ))
            .count(),
        1
    );
}
#[test]
fn preexisting_extended_buttons_are_refused_without_adopting_a_gesture() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (_, layout) = viewport();
    peer.command("motion 40 50");
    for button in [8, 9] {
        peer.command(&format!("button {button} 1"));
        let mut target = Recording {
            start: Instant::now(),
            probe: Arc::new(Probe::default()),
            caps: capabilities(),
        };
        let result = run(&display, peer.window, &layout, &mut target);
        peer.command(&format!("button {button} 0"));
        assert_eq!(result, Err(StopReason::NativeFailure));
        assert!(!target.probe.ready.load(Ordering::Acquire));
        assert!(target.probe.events.lock().unwrap().is_empty());
    }
}
#[test]
fn device_hierarchy_changes_fence_original_capture_and_multiple_masters_refuse_setup() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display.clone(), peer.window, capabilities());
    peer.command("hierarchy");
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), Err(StopReason::InputDevicesChanged));
    assert!(probe.events.lock().unwrap().is_empty());
    let (_, layout) = viewport();
    let mut target = Recording {
        start: Instant::now(),
        probe: Arc::new(Probe::default()),
        caps: capabilities(),
    };
    assert_eq!(
        run(&display, peer.window, &layout, &mut target),
        Err(StopReason::NativeFailure)
    );
    assert!(!target.probe.ready.load(Ordering::Acquire));
    // Peer::drop removes only the master pair this fixture explicitly added.
}
#[test]
fn missing_releases_clear_native_and_remote_held_state_but_extra_bits_never_press() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    names[50] = *b"LFSH";
    let mut decoder = Decoder::new(&names, capabilities());
    for event in [
        Raw {
            kind: 1,
            detail: 38,
            ..Raw::default()
        },
        Raw {
            kind: 3,
            detail: 8,
            ..Raw::default()
        },
    ] {
        assert!(decoder.event(event, &layout).unwrap().is_some());
    }
    let sampler = held::Sampler::new(ClientInstant(1_000_000)).unwrap();
    assert!(!sampler.due(&decoder, ClientInstant(1_000_000 + held::INTERVAL_US - 1)));
    assert!(sampler.due(&decoder, ClientInstant(1_000_000 + held::INTERVAL_US)));
    let mut actual = Snapshot::default();
    actual.keys[38 / 8] |= 1 << (38 % 8);
    actual.keys[50 / 8] |= 1 << (50 % 8);
    actual.buttons[8 / 8] |= 1 << (8 % 8);
    actual.buttons[9 / 8] |= 1 << (9 % 8);
    let Event::HeldState(retained) = decoder.reconcile(&actual).unwrap() else {
        panic!();
    };
    assert!(retained.key(PhysicalKey::new(4).unwrap()));
    assert!(retained.button(PointerButton::Back));
    assert!(!retained.key(PhysicalKey::new(225).unwrap()));
    assert!(!retained.button(PointerButton::Forward));
    // Explicit lost-release fixture: no Release event is delivered to Decoder.
    actual.keys[38 / 8] &= !(1 << (38 % 8));
    actual.buttons[8 / 8] &= !(1 << (8 % 8));
    let Event::HeldState(released) = decoder.reconcile(&actual).unwrap() else {
        panic!();
    };
    assert_eq!(released, HeldState::empty());
    assert!(!decoder.has_held());
    assert!(!sampler.due(&decoder, ClientInstant(u64::MAX)));
    // A later deliberate press is new, not an inferred repeat/resurrected drag.
    assert!(matches!(
        decoder
            .event(
                Raw {
                    kind: 1,
                    detail: 38,
                    ..Raw::default()
                },
                &layout
            )
            .unwrap(),
        Some(Event::Key {
            transition: KeyTransition::Press,
            ..
        })
    ));
}
#[test]
fn snapshots_crossing_any_transition_are_discarded_and_barriers_keep_timestamps_ordered() {
    for kind in 1..=4 {
        assert!(!held::stable(&[Raw {
            kind,
            ..Raw::default()
        }]));
    }
    assert!(held::stable(&[Raw {
        kind: 5,
        ..Raw::default()
    }]));
    assert!(held::stable(&[]));
    let mut timeline = Timeline::new(10, ClientInstant(1_000_000));
    timeline.barrier(11).unwrap();
    timeline.fence(11, ClientInstant(1_001_000)).unwrap();
    timeline.barrier(11).unwrap(); // later event in the SAME native millisecond
    assert_eq!(
        timeline.sample(11, 11, ClientInstant(1_001_200)),
        Ok(ClientInstant(1_001_000))
    );
    assert_eq!(
        timeline.sample(10, 11, ClientInstant(1_002_000)),
        Err(StopReason::Clock)
    );
    assert_eq!(
        timeline.fence(12, ClientInstant(1_002_000)),
        Err(StopReason::Clock)
    );
    assert_eq!(
        timeline.fence(11, ClientInstant(1_000_999)),
        Err(StopReason::Clock)
    );
    let mut wrap = Timeline::new(u32::MAX, ClientInstant(1_000_000));
    wrap.barrier(0).unwrap();
    wrap.fence(0, ClientInstant(1_001_000)).unwrap();
    assert_eq!(
        wrap.sample(0, 0, ClientInstant(1_001_200)),
        Ok(ClientInstant(1_001_000))
    );
}

#[test]
fn native_cadence_cannot_drop_the_final_empty_snapshot_due_to_dispatch_jitter() {
    use fr_client::input::held::HELD_STATE_INTERVAL_US;
    use frd::session_startup::viewer_events::MAX_EVENT_AGE_US;
    assert_eq!(held::INTERVAL_US, HELD_STATE_INTERVAL_US + MAX_EVENT_AGE_US);
    for previous_age in [0, 1, 50_000, MAX_EVENT_AGE_US - 1] {
        for current_age in [0, 1, 50_000, MAX_EVENT_AGE_US - 1] {
            let previous_dispatch = 1_000_000 + previous_age;
            let final_empty_dispatch = 1_000_000 + held::INTERVAL_US + current_age;
            assert!(final_empty_dispatch - previous_dispatch >= HELD_STATE_INTERVAL_US);
        }
    }
}
