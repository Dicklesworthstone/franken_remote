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
fn delayed_reconciled_releases_do_not_end_control_or_emit_another_release() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    for (down, up, detail) in [
        (1, 2, 38),
        (3, 4, 1),
        (3, 4, 2),
        (3, 4, 3),
        (3, 4, 8),
        (3, 4, 9),
    ] {
        let mut decoder = Decoder::new(&names, capabilities());
        let press = Raw {
            kind: down,
            detail,
            ..Raw::default()
        };
        let release = Raw { kind: up, ..press };
        assert!(decoder.event(press, &layout).unwrap().is_some());
        let Event::HeldState(state) = decoder.reconcile(&Snapshot::default()).unwrap() else {
            panic!("expected release-only snapshot");
        };
        assert_eq!(state, HeldState::empty());
        assert!(!decoder.has_held());
        // Retain only the known missing release, without inventing held input
        // or needing another native query during idle. A later actual up is
        // already accounted for by the original snapshot.
        let sampler = held::Sampler::new(ClientInstant(1)).unwrap();
        assert!(!sampler.due(&decoder, ClientInstant(u64::MAX)));
        decoder.reconcile(&Snapshot::default()).unwrap();
        assert!(decoder.event(release, &layout).unwrap().is_none());
        // One reconciled gesture cannot authorize arbitrary unmatched ups.
        assert!(matches!(
            decoder.event(release, &layout),
            Err(StopReason::InitialHeld)
        ));
    }
}

#[test]
fn fresh_key_and_button_presses_retire_only_their_own_reconciled_release() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    names[50] = *b"LFSH";
    for (down, up, detail) in [(1, 2, 38), (3, 4, 1), (3, 4, 8), (3, 4, 9)] {
        let mut decoder = Decoder::new(&names, capabilities());
        let press = Raw {
            kind: down,
            detail,
            ..Raw::default()
        };
        let release = Raw { kind: up, ..press };
        decoder.event(press, &layout).unwrap();
        decoder.reconcile(&Snapshot::default()).unwrap();
        // Unknown snapshot bits cannot create a press or release allowance.
        decoder
            .reconcile(&Snapshot {
                keys: [255; 32],
                buttons: [255; 32],
            })
            .unwrap();
        assert!(!decoder.has_held());
        assert!(matches!(
            decoder.event(
                Raw {
                    kind: 2,
                    detail: 50,
                    ..Raw::default()
                },
                &layout
            ),
            Err(StopReason::InitialHeld)
        ));
        // A fresh physical gesture uses the normal decoder on both edges;
        // old cleanup must never swallow its release and strand remote input.
        assert!(decoder.event(press, &layout).unwrap().is_some());
        assert!(decoder.has_held());
        assert!(decoder.event(release, &layout).unwrap().is_some());
        assert!(!decoder.has_held());
        assert!(matches!(
            decoder.event(release, &layout),
            Err(StopReason::InitialHeld)
        ));
    }
}

#[test]
fn fresh_presses_preserve_other_reconciled_gestures_but_not_a_new_capture() {
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    names[50] = *b"LFSH";
    let (_, layout) = viewport();
    let mut decoder = Decoder::new(&names, capabilities());
    for (kind, detail) in [(1, 38), (1, 50), (3, 1), (3, 8)] {
        assert!(
            decoder
                .event(
                    Raw {
                        kind,
                        detail,
                        ..Raw::default()
                    },
                    &layout
                )
                .unwrap()
                .is_some()
        );
    }
    decoder.reconcile(&held::Snapshot::default()).unwrap();
    for (kind, detail) in [(1, 38), (3, 1)] {
        assert!(
            decoder
                .event(
                    Raw {
                        kind,
                        detail,
                        ..Raw::default()
                    },
                    &layout
                )
                .unwrap()
                .is_some()
        );
    }
    for (kind, detail) in [(2, 50), (4, 8)] {
        assert!(matches!(
            decoder.event(
                Raw {
                    kind,
                    detail,
                    ..Raw::default()
                },
                &layout
            ),
            Ok(None)
        ));
    }
    assert!(decoder.has_held());
    for (kind, detail) in [(2, 38), (4, 1)] {
        assert!(
            decoder
                .event(
                    Raw {
                        kind,
                        detail,
                        ..Raw::default()
                    },
                    &layout
                )
                .unwrap()
                .is_some()
        );
    }
    assert!(!decoder.has_held());

    let mut replacement = Decoder::new(&names, capabilities());
    for (kind, detail) in [(2, 50), (4, 8)] {
        assert!(matches!(
            replacement.event(
                Raw {
                    kind,
                    detail,
                    ..Raw::default()
                },
                &layout
            ),
            Err(StopReason::InitialHeld)
        ));
    }
}

fn capture_after_reconciliation(
    decoder: &mut Decoder,
    events: &[Raw],
) -> Result<Option<CapturedBatch>, StopReason> {
    let mut timeline = Timeline::new(1000, ClientInstant(1_000_000));
    timeline.barrier(1010)?;
    let target = Recording {
        start: Instant::now(),
        probe: Arc::new(Probe::default()),
        caps: capabilities(),
    };
    captured_batch(
        events,
        &mut timeline,
        1010,
        ClientInstant(1_010_000),
        &target,
        None,
        decoder,
    )
}

#[test]
fn reconciled_releases_before_the_snapshot_time_fence_do_not_advance_input_time() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    let mut decoder = Decoder::new(&names, capabilities());
    for (kind, detail) in [(1, 38), (3, 8)] {
        decoder
            .event(
                Raw {
                    kind,
                    detail,
                    time: 990,
                    ..Raw::default()
                },
                &layout,
            )
            .unwrap();
    }
    decoder.reconcile(&Snapshot::default()).unwrap();
    let events = [
        Raw {
            kind: 2,
            detail: 38,
            time: 999,
            ..Raw::default()
        },
        Raw {
            kind: 4,
            detail: 8,
            time: 999,
            ..Raw::default()
        },
        Raw {
            kind: 5,
            time: 1009,
            ..Raw::default()
        },
    ];
    let captured = capture_after_reconciliation(&mut decoder, &events)
        .unwrap()
        .unwrap();
    assert_eq!(captured.at[0], None);
    assert_eq!(captured.at[1], None);
    assert_eq!(captured.at[2], Some(ClientInstant(1_008_000)));
    assert!(!decoder.has_held());
    // Keep the original raw events for snapshot-stability and escape/lifecycle
    // inspection; ignoring an up must not turn a crossing batch into evidence.
    assert!(!held::stable(&events));
}

#[test]
fn fresh_down_and_up_in_one_batch_cannot_consume_retired_gesture_custody() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    for (down, up, detail) in [(1, 2, 38), (3, 4, 1), (3, 4, 8)] {
        let mut decoder = Decoder::new(&names, capabilities());
        decoder
            .event(
                Raw {
                    kind: down,
                    detail,
                    ..Raw::default()
                },
                &layout,
            )
            .unwrap();
        decoder.reconcile(&Snapshot::default()).unwrap();
        let events = [
            Raw {
                kind: down,
                detail,
                time: 1001,
                ..Raw::default()
            },
            Raw {
                kind: up,
                detail,
                time: 1002,
                ..Raw::default()
            },
        ];
        let captured = capture_after_reconciliation(&mut decoder, &events)
            .unwrap()
            .unwrap();
        assert_eq!(captured.at[0], Some(ClientInstant(1_000_000)));
        assert_eq!(captured.at[1], Some(ClientInstant(1_001_000)));
        assert!(matches!(
            decoder.event(events[0], &layout).unwrap(),
            Some(
                Event::Key {
                    transition: KeyTransition::Press,
                    ..
                } | Event::Positioned {
                    action: PositionedAction::Button { pressed: true, .. },
                    ..
                }
            )
        ));
        assert!(decoder.has_held());
        assert!(matches!(
            decoder.event(events[1], &layout).unwrap(),
            Some(
                Event::Key {
                    transition: KeyTransition::Release,
                    ..
                } | Event::Positioned {
                    action: PositionedAction::Button { pressed: false, .. },
                    ..
                }
            )
        ));
        assert!(!decoder.has_held());
    }
}

#[test]
fn stale_unowned_and_wheel_releases_cannot_bypass_the_native_time_fence() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    names[50] = *b"LFSH";
    for (kind, detail) in [(2, 50), (4, 1), (4, 4), (4, 5), (4, 6), (4, 7)] {
        let mut decoder = Decoder::new(&names, capabilities());
        // A different known key and genuine wheel activity do not authorize
        // an unowned key/button up, or either edge of a scroll command.
        decoder
            .event(
                Raw {
                    kind: 1,
                    detail: 38,
                    ..Raw::default()
                },
                &layout,
            )
            .unwrap();
        decoder
            .event(
                Raw {
                    kind: 3,
                    detail: 4,
                    ..Raw::default()
                },
                &layout,
            )
            .unwrap();
        decoder.reconcile(&Snapshot::default()).unwrap();
        let event = Raw {
            kind,
            detail,
            time: 999,
            ..Raw::default()
        };
        assert!(matches!(
            capture_after_reconciliation(&mut decoder, &[event]),
            Err(StopReason::Clock)
        ));
        assert!(decoder.released_keys[38]);
    }
    // Even with old cleanup history, an old up arriving after a fresh down is
    // an ambiguous reordered gesture, never permission to release that down.
    let mut decoder = Decoder::new(&names, capabilities());
    decoder
        .event(
            Raw {
                kind: 1,
                detail: 38,
                ..Raw::default()
            },
            &layout,
        )
        .unwrap();
    decoder.reconcile(&Snapshot::default()).unwrap();
    let reordered = [
        Raw {
            kind: 1,
            detail: 38,
            time: 1001,
            ..Raw::default()
        },
        Raw {
            kind: 2,
            detail: 38,
            time: 999,
            ..Raw::default()
        },
    ];
    assert!(matches!(
        capture_after_reconciliation(&mut decoder, &reordered),
        Err(StopReason::Clock)
    ));
    assert!(!decoder.has_held()); // No part of the refused batch was dispatched.
}

#[test]
fn old_release_then_fresh_gesture_in_one_batch_keeps_both_new_edges() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    for (down, up, detail) in [(1, 2, 38), (3, 4, 1), (3, 4, 8)] {
        let mut decoder = Decoder::new(&names, capabilities());
        let press = Raw {
            kind: down,
            detail,
            time: 990,
            ..Raw::default()
        };
        decoder.event(press, &layout).unwrap();
        decoder.reconcile(&Snapshot::default()).unwrap();
        let events = [
            Raw {
                kind: up,
                time: 999,
                ..press
            },
            Raw {
                time: 1001,
                ..press
            },
            Raw {
                kind: up,
                time: 1002,
                ..press
            },
        ];
        let captured = capture_after_reconciliation(&mut decoder, &events)
            .unwrap()
            .unwrap();
        assert_eq!(
            captured.at[..3],
            [
                None,
                Some(ClientInstant(1_000_000)),
                Some(ClientInstant(1_001_000))
            ]
        );
        // Follow the real dispatch rule: the proved old up has no timestamp
        // and never reaches the decoder; the new gesture retains both edges.
        for (index, event) in events.into_iter().enumerate() {
            if captured.at[index].is_some() {
                assert!(decoder.event(event, &layout).unwrap().is_some());
            }
        }
        assert!(!decoder.has_held());
        assert!(!held::stable(&events));
    }
}

#[test]
fn snapshots_crossing_any_transition_are_discarded_and_barriers_keep_timestamps_ordered() {
    for kind in [1, 2, 3, 4, 13, 14, 15, 16, 17, 18] {
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
