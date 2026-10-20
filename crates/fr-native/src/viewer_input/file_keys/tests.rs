use super::*;
fn names() -> [[u8; 4]; 256] {
    let mut names = [[0; 4]; 256];
    for (slot, name) in NAMES.iter().enumerate() {
        names[slot + 8] = *name;
    }
    names
}
fn key(slot: usize, down: bool) -> Raw {
    Raw {
        kind: if down { 1 } else { 2 },
        detail: u32::try_from(slot + 8).unwrap(),
        ..Raw::default()
    }
}
fn modifiers(keys: &mut Shortcuts, enabled: bool) {
    for slot in [0, 2, 4] {
        assert_eq!(keys.event(key(slot, true), enabled), Route::Remote);
    }
}
#[test]
fn every_modifier_side_and_order_selects_each_command_once_per_gesture() {
    for control in 0..2 {
        for alt in 2..4 {
            for shift in 4..6 {
                for order in [
                    [control, alt, shift],
                    [control, shift, alt],
                    [alt, control, shift],
                    [alt, shift, control],
                    [shift, control, alt],
                    [shift, alt, control],
                ] {
                    for (slot, command) in [
                        (6, Command::Pause),
                        (7, Command::Resume),
                        (8, Command::Cancel),
                    ] {
                        let mut keys = Shortcuts::new(&names()).unwrap();
                        for modifier in order {
                            assert_eq!(keys.event(key(modifier, true), true), Route::Remote);
                        }
                        assert_eq!(
                            keys.event(key(slot, true), true),
                            Route::Local(Some(command))
                        );
                        for _ in 0..4 {
                            assert_eq!(keys.event(key(slot, true), true), Route::Local(None));
                        }
                        // Releasing the modifiers first does not leak the function-key up.
                        for modifier in order {
                            assert_eq!(keys.event(key(modifier, false), true), Route::Remote);
                        }
                        assert_eq!(keys.event(key(slot, false), true), Route::Local(None));
                        assert!(!keys.has_held());
                    }
                }
            }
        }
    }
}
#[test]
fn disabled_controls_preserve_every_key_transition() {
    let mut keys = Shortcuts::new(&names()).unwrap();
    for slot in [0, 2, 4, 6, 7, 8] {
        assert_eq!(keys.event(key(slot, true), false), Route::Remote);
        assert_eq!(keys.event(key(slot, true), false), Route::Remote);
    }
    for slot in [0, 2, 4, 6, 7, 8] {
        assert_eq!(keys.event(key(slot, false), false), Route::Remote);
    }
    assert!(!keys.has_held());
}
#[test]
fn a_function_key_pressed_before_its_modifiers_stays_remote_until_release() {
    for slot in 6..9 {
        let mut keys = Shortcuts::new(&names()).unwrap();
        assert_eq!(keys.event(key(slot, true), true), Route::Remote);
        modifiers(&mut keys, true);
        assert_eq!(keys.event(key(slot, true), true), Route::Remote);
        assert_eq!(keys.event(key(slot, false), true), Route::Remote);
        assert!(matches!(
            keys.event(key(slot, true), true),
            Route::Local(Some(_))
        ));
    }
}
#[test]
fn binding_while_a_remote_chord_is_held_does_not_steal_repeat_or_release() {
    for slot in 6..9 {
        let mut keys = Shortcuts::new(&names()).unwrap();
        modifiers(&mut keys, false);
        assert_eq!(keys.event(key(slot, true), false), Route::Remote);
        assert_eq!(keys.event(key(slot, true), true), Route::Remote);
        assert_eq!(keys.event(key(slot, false), true), Route::Remote);
        assert!(matches!(
            keys.event(key(slot, true), true),
            Route::Local(Some(_))
        ));
    }
}
#[test]
fn local_gesture_custody_survives_disabled_or_terminal_binding() {
    let mut keys = Shortcuts::new(&names()).unwrap();
    modifiers(&mut keys, true);
    assert_eq!(
        keys.event(key(6, true), true),
        Route::Local(Some(Command::Pause))
    );
    for modifier in [0, 2, 4] {
        assert_eq!(keys.event(key(modifier, false), false), Route::Remote);
    }
    assert_eq!(keys.event(key(6, true), false), Route::Local(None));
    assert_eq!(keys.event(key(6, false), false), Route::Local(None));
}
#[test]
fn stable_snapshots_only_clear_and_keep_a_delayed_local_release_local() {
    let mut keys = Shortcuts::new(&names()).unwrap();
    modifiers(&mut keys, true);
    assert_eq!(
        keys.event(key(6, true), true),
        Route::Local(Some(Command::Pause))
    );
    keys.reconcile(&[0; 32]);
    assert!(!keys.has_held());
    keys.reconcile(&[255; 32]);
    assert!(!keys.has_held());
    assert_eq!(keys.event(key(6, false), true), Route::Local(None));
    assert_eq!(keys.event(key(7, true), true), Route::Remote);
    assert_eq!(keys.event(key(7, false), true), Route::Remote);
    modifiers(&mut keys, true);
    assert_eq!(
        keys.event(key(7, true), true),
        Route::Local(Some(Command::Resume))
    );
}
#[test]
fn missing_ambiguous_or_invalid_physical_maps_never_select_alternative_keys() {
    for slot in 6..9 {
        let mut mapping = names();
        mapping[slot + 8] = [0; 4];
        assert!(Shortcuts::new(&mapping).is_none());
    }
    for group in 0..3 {
        let mut mapping = names();
        mapping[group * 2 + 8] = [0; 4];
        mapping[group * 2 + 9] = [0; 4];
        assert!(Shortcuts::new(&mapping).is_none());
    }
    for name in NAMES {
        for code in [1, 99] {
            let mut mapping = names();
            mapping[code] = name;
            assert!(Shortcuts::new(&mapping).is_none());
        }
    }
}
#[test]
fn incomplete_chords_and_nonkeys_remain_remote() {
    for omitted in [0, 2, 4] {
        let mut keys = Shortcuts::new(&names()).unwrap();
        for slot in [0, 2, 4] {
            if slot != omitted {
                keys.event(key(slot, true), true);
            }
        }
        for slot in 6..9 {
            assert_eq!(keys.event(key(slot, true), true), Route::Remote);
            assert_eq!(keys.event(key(slot, false), true), Route::Remote);
        }
    }
    let mut keys = Shortcuts::new(&names()).unwrap();
    modifiers(&mut keys, true);
    for kind in 3..=12 {
        assert_eq!(
            keys.event(
                Raw {
                    kind,
                    ..key(6, true)
                },
                true
            ),
            Route::Remote
        );
    }
    assert_eq!(
        keys.event(
            Raw {
                detail: 255,
                ..key(6, true)
            },
            true
        ),
        Route::Remote
    );
}
#[test]
fn releasing_one_side_does_not_forget_the_other_held_modifier() {
    let mut keys = Shortcuts::new(&names()).unwrap();
    for slot in 0..6 {
        keys.event(key(slot, true), true);
    }
    for slot in [0, 2, 4] {
        keys.event(key(slot, false), true);
    }
    assert_eq!(
        keys.event(key(8, true), true),
        Route::Local(Some(Command::Cancel))
    );
}
