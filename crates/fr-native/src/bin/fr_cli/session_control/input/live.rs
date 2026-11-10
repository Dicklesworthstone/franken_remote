//! Dispatch happens only on the original controlled session's synchronous turn.
use super::{Candidate, Shared};
use crate::robot_input::Operation;
use fr_client::input::Action;
use fr_core::input::{DesktopPoint, KeyTransition, PhysicalKey, PointerButton, ScrollUnit};
use frd::session_startup::{ControlledViewer, ControlledViewerError};
use std::time::Instant;

impl Operation {
    fn committed_text(&self, index: u8) -> Option<&str> {
        match self {
            Self::Text { text } if index == 0 => Some(text.as_str()),
            _ => None,
        }
    }
    fn action(&self, index: u8) -> Option<Action<'static>> {
        match *self {
            Self::Scroll { x, y, .. } => {
                let (dx, dy) = self.scroll_lines(index)?;
                Some(Action::Scroll {
                    position: DesktopPoint { x, y },
                    x: dx * fr_core::input_submission::scroll::LINE,
                    y: dy * fr_core::input_submission::scroll::LINE,
                    unit: ScrollUnit::Lines,
                })
            }
            Self::ModifiedScroll {
                modifiers,
                x,
                y,
                dx,
                dy,
            } if self.valid() => {
                Self::Scroll { x, y, dx, dy }.action_with_modifiers(modifiers, index)
            }
            Self::Click { button, x, y } if index < 2 => Some(Action::Button {
                button: match button {
                    1 => PointerButton::Primary,
                    2 => PointerButton::Secondary,
                    3 => PointerButton::Middle,
                    4 => PointerButton::Back,
                    5 => PointerButton::Forward,
                    _ => return None,
                },
                pressed: index == 0,
                position: DesktopPoint { x, y },
            }),
            Self::Drag {
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            } if self.valid() && index < 2 => {
                // A reliable Button includes its own atomic absolute-position
                // barrier. The release moves with the button still held before
                // releasing it; it is not a second click or a lossy pointer hop.
                let (x, y) = if index == 0 {
                    (from_x, from_y)
                } else {
                    (to_x, to_y)
                };
                Self::Click { button, x, y }.action(index)
            }
            Self::ModifiedClick {
                modifiers,
                button,
                x,
                y,
            } if self.valid() => {
                Self::Click { button, x, y }.action_with_modifiers(modifiers, index)
            }
            Self::ModifiedDrag {
                modifiers,
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            } if self.valid() => Self::Drag {
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            }
            .action_with_modifiers(modifiers, index),
            Self::KeyTap { usage } if index < 2 => Some(Action::Key {
                key: PhysicalKey::new(usage)?,
                transition: if index == 0 {
                    KeyTransition::Press
                } else {
                    KeyTransition::Release
                },
            }),
            Self::KeyChord { modifiers, usage } if self.valid() && index < self.count() => {
                let release_start = self.count() / 2;
                let pressed = index < release_start;
                // Press the canonical modifier set, then the key. Mirror that
                // order for release so every modifier spans the entire key tap.
                let ordinal = if pressed {
                    index
                } else {
                    2 * release_start - index - 1
                };
                let usage = (224_u16..=231)
                    .filter(|key| modifiers & (1 << (*key - 224)) != 0)
                    .chain(std::iter::once(usage))
                    .nth(usize::from(ordinal))?;
                Some(Action::Key {
                    key: PhysicalKey::new(usage)?,
                    transition: if pressed {
                        KeyTransition::Press
                    } else {
                        KeyTransition::Release
                    },
                })
            }
            _ => None,
        }
    }
    fn action_with_modifiers(&self, modifiers: u8, index: u8) -> Option<Action<'static>> {
        let held = u8::try_from(modifiers.count_ones()).ok()?;
        let release_start = held.checked_add(self.count())?;
        let total = release_start.checked_add(held)?;
        if index >= total {
            return None;
        }
        if (held..release_start).contains(&index) {
            return self.action(index - held);
        }
        // Each modifier remains held across every reliable body action.
        // Reverse release order mirrors the canonical presses, just as a chord.
        let pressed = index < held;
        let ordinal = if pressed { index } else { total - index - 1 };
        let usage = (224_u16..=231)
            .filter(|key| modifiers & (1 << (*key - 224)) != 0)
            .nth(usize::from(ordinal))?;
        Some(Action::Key {
            key: PhysicalKey::new(usage)?,
            transition: if pressed {
                KeyTransition::Press
            } else {
                KeyTransition::Release
            },
        })
    }
}
impl Shared {
    #[allow(clippy::too_many_lines)]
    pub fn turn(&self, viewer: &mut ControlledViewer, focused: bool) {
        let mut state = self.locked();
        if state.stop.is_none() {
            state.stop = Some(viewer.control());
        }
        if state.stopped || state.ledger.is_closed() {
            viewer.close();
            state.retire();
            return;
        }
        let at = Instant::now();
        let Ok(now) = state.now(at) else {
            viewer.close();
            state.retire();
            return;
        };
        if state.ledger.expire(now).is_err() {
            viewer.close();
            state.retire();
            return;
        }
        state.finish_if_retained();
        if state.stopped {
            viewer.close();
            return;
        }
        state.live = if focused {
            viewer
                .input_observation()
                .ok()
                .map(|(view, sample)| Candidate {
                    view,
                    sample,
                    at,
                    bounds: viewer.granted_input_bounds(),
                    capabilities: viewer.granted_capabilities(),
                })
        } else {
            None
        };
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        let id = pending.request.request_id;
        let ready = state.ledger.ready(id, now);
        match ready {
            Ok(false) => {
                state.finish_if_retained();
                return;
            }
            Err(_) => {
                viewer.close();
                state.retire();
                return;
            }
            Ok(true) => {}
        }
        let Some(pending) = state.pending.as_ref() else {
            return;
        };
        let current = state.live;
        let valid = focused
            && current.is_some_and(|live| {
                live.view == pending.original.view
                    && live.bounds == pending.original.bounds
                    && live.capabilities == pending.original.capabilities
            })
            && pending
                .original
                .age(Instant::now())
                .is_some_and(|age| age < u64::from(pending.request.max_observation_age_ms) * 1000);
        if !valid {
            let _ = state.ledger.reject(id);
            state.finish_if_retained();
            return;
        }
        let binding = viewer.input_result_binding();
        let encoded = if let Some(text) = pending.request.operation.committed_text(pending.encoded)
        {
            viewer.action(Action::Text(text))
        } else if let Some(position) = pending.request.operation.pointer(pending.encoded) {
            viewer.pointer(position)
        } else if let Some(action) = pending.request.operation.action(pending.encoded) {
            viewer.action(action)
        } else {
            viewer.close();
            state.retire();
            return;
        };
        match encoded {
            Ok(encoded) => {
                if state
                    .ledger
                    .encoded_in(id, binding, encoded.space, encoded.sequence)
                    .is_err()
                {
                    // Sending became possible before ledger recording. Fence
                    // immediately; the ledger preserves that uncertainty.
                    viewer.close();
                    state.retire();
                    return;
                }
                if let Some(pending) = state.pending.as_mut() {
                    pending.encoded += 1;
                }
            }
            Err(ControlledViewerError::Backpressure) => {} // no ID encoded; original observation/deadline still apply
            Err(_) => {
                let _ = state.ledger.reject(id);
                state.finish_if_retained();
            }
        }
        state.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn move_encodes_one_pointer_without_synthesizing_a_reliable_action() {
        let operation = Operation::Move { x: -700, y: 500 };
        assert!(operation.valid());
        assert_eq!(operation.count(), 1);
        assert_eq!(operation.pointer(0), Some(DesktopPoint { x: -700, y: 500 }));
        for index in 0..=u8::MAX {
            assert!(operation.action(index).is_none());
            if index != 0 {
                assert!(operation.pointer(index).is_none());
            }
        }
        assert!(
            Operation::Click {
                button: 1,
                x: -700,
                y: 500
            }
            .pointer(0)
            .is_none()
        );
    }

    #[test]
    fn robot_scroll_preserves_every_line_within_the_native_submission_bound() {
        use fr_core::input_submission::scroll::{LINE, LineScroll, MAX_STEPS, WheelDirection};

        for (dx, dy) in [
            (0, 1),
            (32, 0),
            (33, 0),
            (20, -20),
            (0, -120),
            (120, 120),
            (-120, 120),
            (120, -120),
            (-120, -120),
        ] {
            let operation = Operation::Scroll {
                x: -700,
                y: 500,
                dx,
                dy,
            };
            assert!(operation.valid());
            assert!(operation.count() <= fr_client::robot::input::native::MAX_ACTIONS);
            let mut distance = (0, 0);
            let mut directions = Vec::new();
            for index in 0..operation.count() {
                let Some(Action::Scroll {
                    position,
                    x,
                    y,
                    unit,
                }) = operation.action(index)
                else {
                    panic!("every declared scroll action must exist");
                };
                assert_eq!(position, DesktopPoint { x: -700, y: 500 });
                assert_eq!(unit, ScrollUnit::Lines);
                // This is the actual X11 executor's pre-submission gate. The
                // original single 120-line action failed here before moving.
                let native = LineScroll::new(x, y).expect("bounded native wheel action");
                assert!(native.native_operations() <= 1 + 2 * MAX_STEPS);
                assert!(native.native_operations() > 1);
                distance.0 += x / LINE;
                distance.1 += y / LINE;
                directions.extend(native.steps());
            }
            assert_eq!(distance, (dx, dy));
            let expected: Vec<_> = std::iter::repeat_n(
                if dx < 0 {
                    WheelDirection::Left
                } else {
                    WheelDirection::Right
                },
                dx.unsigned_abs() as usize,
            )
            .chain(std::iter::repeat_n(
                if dy < 0 {
                    WheelDirection::Up
                } else {
                    WheelDirection::Down
                },
                dy.unsigned_abs() as usize,
            ))
            .collect();
            assert_eq!(
                directions, expected,
                "horizontal motion precedes vertical motion"
            );
            assert!(operation.action(operation.count()).is_none());
            assert!(operation.action(u8::MAX).is_none());
        }
    }

    #[test]
    fn invalid_scrolls_cannot_expand_into_valid_partial_input() {
        for (dx, dy) in [(0, 0), (121, 0), (0, -121), (i32::MIN, i32::MAX)] {
            let operation = Operation::Scroll { x: 0, y: 0, dx, dy };
            assert!(!operation.valid());
            for index in 0..=u8::MAX {
                assert!(operation.action(index).is_none());
            }
        }
    }

    fn key_step(operation: &Operation, index: u8) -> (u16, KeyTransition) {
        let Some(Action::Key { key, transition }) = operation.action(index) else {
            panic!("a chord emits only bounded physical key transitions");
        };
        (key.usage(), transition)
    }

    #[test]
    fn committed_text_lowers_once_without_key_or_pointer_fallback() {
        use crate::robot_input::text::Text;
        use fr_core::input_submission::{Capabilities, Capability};
        let source = "e\u{301}é漢🙂\r\n\t";
        let operation = Operation::Text {
            text: Text::new(source).unwrap(),
        };
        assert!(operation.valid());
        assert_eq!(operation.count(), 1);
        assert_eq!(operation.committed_text(0), Some(source));
        assert!(operation.supported(Capabilities::default().with(Capability::Text)));
        assert!(!operation.supported(Capabilities::default().with(Capability::Keys)));
        for index in 0..=u8::MAX {
            assert!(operation.action(index).is_none());
            assert!(operation.pointer(index).is_none());
            if index != 0 {
                assert!(operation.committed_text(index).is_none());
            }
        }
        assert!(Operation::KeyTap { usage: 4 }.committed_text(0).is_none());
    }

    #[test]
    fn shortcut_keeps_modifiers_held_through_the_key_and_releases_in_reverse() {
        let chord = Operation::KeyChord {
            modifiers: 0b0000_0011,
            usage: 19,
        };
        let expected = [
            (224, KeyTransition::Press),
            (225, KeyTransition::Press),
            (19, KeyTransition::Press),
            (19, KeyTransition::Release),
            (225, KeyTransition::Release),
            (224, KeyTransition::Release),
        ];
        for (index, step) in expected.into_iter().enumerate() {
            assert_eq!(key_step(&chord, u8::try_from(index).unwrap()), step);
        }
        assert!(chord.action(6).is_none());
    }

    #[test]
    fn maximum_chord_has_exactly_ten_transitions_and_no_held_keys_after_completion() {
        let chord = Operation::KeyChord {
            modifiers: 0b1100_0101,
            usage: 164,
        };
        let expected = [224, 226, 230, 231, 164, 164, 231, 230, 226, 224];
        assert_eq!(chord.count(), 10);
        let mut held = BTreeSet::new();
        for (index, usage) in expected.into_iter().enumerate() {
            let (actual, transition) = key_step(&chord, u8::try_from(index).unwrap());
            assert_eq!(actual, usage);
            if index < 5 {
                assert_eq!(transition, KeyTransition::Press);
                assert!(held.insert(actual));
            } else {
                assert_eq!(transition, KeyTransition::Release);
                assert!(held.remove(&actual));
            }
            if index == 4 || index == 5 {
                assert!(
                    [224, 226, 230, 231]
                        .iter()
                        .all(|usage| held.contains(usage))
                );
            }
        }
        assert!(held.is_empty());
        for index in 10..=255 {
            assert!(chord.action(index).is_none());
        }
    }

    #[test]
    fn malformed_raw_chords_never_reach_the_action_encoder() {
        for chord in [
            Operation::KeyChord {
                modifiers: 0,
                usage: 4,
            },
            Operation::KeyChord {
                modifiers: 0b0001_1111,
                usage: 4,
            },
            Operation::KeyChord {
                modifiers: 255,
                usage: 4,
            },
            Operation::KeyChord {
                modifiers: 1,
                usage: 224,
            },
            Operation::KeyChord {
                modifiers: 1,
                usage: 165,
            },
        ] {
            for index in 0..=255 {
                assert!(chord.action(index).is_none());
            }
        }
    }

    #[test]
    fn a_drag_uses_only_a_press_at_source_and_reliable_release_at_destination() {
        let a = DesktopPoint { x: -800, y: -200 };
        let b = DesktopPoint { x: 799, y: 599 };
        for button in 1..=5 {
            let drag = Operation::Drag {
                button,
                from_x: a.x,
                from_y: a.y,
                to_x: b.x,
                to_y: b.y,
            };
            assert_eq!(drag.count(), 2);
            for (index, pressed, position) in [(0, true, a), (1, false, b)] {
                let Some(Action::Button {
                    button: actual,
                    pressed: down,
                    position: point,
                }) = drag.action(index)
                else {
                    panic!("a drag must emit two reliable button transitions");
                };
                assert_eq!(actual as u8, button);
                assert_eq!(down, pressed);
                assert_eq!(point, position);
            }
            for index in 2..=255 {
                assert!(drag.action(index).is_none());
            }
        }
    }

    #[test]
    fn malformed_or_stationary_drags_do_not_encode_either_half() {
        for (button, to_x, to_y) in [(0, 2, 3), (6, 2, 3), (255, 2, 3), (1, -100, 20)] {
            let drag = Operation::Drag {
                button,
                from_x: -100,
                from_y: 20,
                to_x,
                to_y,
            };
            for index in 0..=255 {
                assert!(drag.action(index).is_none());
            }
        }
    }

    fn modified_gestures(modifiers: u8, button: u8) -> [Operation; 2] {
        [
            Operation::ModifiedClick {
                modifiers,
                button,
                x: -800,
                y: -200,
            },
            Operation::ModifiedDrag {
                modifiers,
                button,
                from_x: -800,
                from_y: -200,
                to_x: 799,
                to_y: 599,
            },
        ]
    }

    #[test]
    fn modified_gestures_keep_every_modifier_held_through_both_button_barriers() {
        for modifiers in 1_u8..=255 {
            if modifiers.count_ones() > 4 {
                continue;
            }
            let usages: Vec<_> = (224_u16..=231)
                .filter(|usage| modifiers & (1 << (*usage - 224)) != 0)
                .collect();
            let count = u8::try_from(usages.len()).unwrap();
            for button in 1..=5 {
                for operation in modified_gestures(modifiers, button) {
                    assert_eq!(operation.count(), 2 * count + 2);
                    let mut held = BTreeSet::new();
                    for index in 0..operation.count() {
                        assert!(operation.pointer(index).is_none());
                        match operation
                            .action(index)
                            .expect("every bounded action exists")
                        {
                            Action::Key { key, transition } => {
                                if index < count {
                                    assert_eq!(key.usage(), usages[usize::from(index)]);
                                    assert_eq!(transition, KeyTransition::Press);
                                    assert!(held.insert(key.usage()));
                                } else {
                                    let ordinal = operation.count() - index - 1;
                                    assert_eq!(key.usage(), usages[usize::from(ordinal)]);
                                    assert_eq!(transition, KeyTransition::Release);
                                    assert!(held.remove(&key.usage()));
                                }
                            }
                            Action::Button {
                                button: actual,
                                pressed,
                                position,
                            } => {
                                assert_eq!(actual as u8, button);
                                assert_eq!(held.iter().copied().collect::<Vec<_>>(), usages);
                                assert!(index == count || index == count + 1);
                                assert_eq!(pressed, index == count);
                                let expected = if !pressed
                                    && matches!(operation, Operation::ModifiedDrag { .. })
                                {
                                    DesktopPoint { x: 799, y: 599 }
                                } else {
                                    DesktopPoint { x: -800, y: -200 }
                                };
                                assert_eq!(position, expected);
                            }
                            _ => panic!("only physical modifiers and reliable buttons are allowed"),
                        }
                    }
                    assert!(held.is_empty());
                    for index in operation.count()..=u8::MAX {
                        assert!(operation.action(index).is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_modified_gestures_never_emit_even_the_first_modifier() {
        for (modifiers, button) in [(0, 1), (31, 1), (255, 1), (1, 0), (1, 6), (1, 255)] {
            for operation in modified_gestures(modifiers, button) {
                assert!(!operation.valid());
                for index in 0..=u8::MAX {
                    assert!(operation.action(index).is_none());
                }
            }
        }
        let stationary = Operation::ModifiedDrag {
            modifiers: 1,
            button: 1,
            from_x: -100,
            from_y: 20,
            to_x: -100,
            to_y: 20,
        };
        assert!(!stationary.valid());
        for index in 0..=u8::MAX {
            assert!(stationary.action(index).is_none());
        }
    }

    #[test]
    fn modified_scroll_keeps_modifiers_held_across_every_native_wheel_batch() {
        use fr_core::input_submission::scroll::{LINE, LineScroll, MAX_STEPS, WheelDirection};

        for (modifiers, dx, dy, total) in [
            (1, 0, -3, 3),
            (0b1000_0001, 20, -20, 6),
            (1, -120, 120, 10),
            (0b1000_0001, 120, -72, 10),
            (0b1000_0011, -120, 8, 10),
            (0b1100_0011, 32, -32, 10),
        ] {
            let operation = Operation::ModifiedScroll {
                modifiers,
                x: -700,
                y: 500,
                dx,
                dy,
            };
            assert!(operation.valid());
            assert_eq!(operation.count(), total);
            let usages: Vec<_> = (224_u16..=231)
                .filter(|usage| modifiers & (1 << (*usage - 224)) != 0)
                .collect();
            let count = u8::try_from(usages.len()).unwrap();
            let mut held = BTreeSet::new();
            let mut distance = (0, 0);
            let mut directions = Vec::new();
            for index in 0..operation.count() {
                assert!(operation.pointer(index).is_none());
                match operation
                    .action(index)
                    .expect("every bounded action exists")
                {
                    Action::Key { key, transition } => {
                        if index < count {
                            assert_eq!(key.usage(), usages[usize::from(index)]);
                            assert_eq!(transition, KeyTransition::Press);
                            assert!(held.insert(key.usage()));
                        } else {
                            let ordinal = operation.count() - index - 1;
                            assert_eq!(key.usage(), usages[usize::from(ordinal)]);
                            assert_eq!(transition, KeyTransition::Release);
                            assert!(held.remove(&key.usage()));
                        }
                    }
                    Action::Scroll {
                        position,
                        x,
                        y,
                        unit,
                    } => {
                        assert_eq!(held.iter().copied().collect::<Vec<_>>(), usages);
                        assert_eq!(position, DesktopPoint { x: -700, y: 500 });
                        assert_eq!(unit, ScrollUnit::Lines);
                        let native = LineScroll::new(x, y).expect("bounded native wheel action");
                        assert!(native.native_operations() <= 1 + 2 * MAX_STEPS);
                        assert!(native.native_operations() > 1);
                        distance.0 += x / LINE;
                        distance.1 += y / LINE;
                        directions.extend(native.steps());
                    }
                    _ => panic!("a modified scroll emits only physical keys and reliable wheels"),
                }
            }
            assert_eq!(held, BTreeSet::new());
            assert_eq!(distance, (dx, dy));
            let expected: Vec<_> = std::iter::repeat_n(
                if dx < 0 {
                    WheelDirection::Left
                } else {
                    WheelDirection::Right
                },
                usize::try_from(dx.unsigned_abs()).unwrap(),
            )
            .chain(std::iter::repeat_n(
                if dy < 0 {
                    WheelDirection::Up
                } else {
                    WheelDirection::Down
                },
                usize::try_from(dy.unsigned_abs()).unwrap(),
            ))
            .collect();
            assert_eq!(
                directions, expected,
                "horizontal steps still precede vertical steps"
            );
            for index in operation.count()..=u8::MAX {
                assert!(operation.action(index).is_none());
            }
        }
    }

    #[test]
    fn invalid_modified_scrolls_never_emit_a_modifier_or_partial_wheel_sequence() {
        for (modifiers, dx, dy) in [
            (0, 0, 1),
            (31, 0, 1),
            (255, 0, 1),
            (1, 0, 0),
            (1, 121, 0),
            (1, 0, -121),
            (1, i32::MIN, i32::MAX),
            (255, i32::MIN, i32::MAX),
            (3, 120, 73),
            (7, 120, 9),
            (15, 32, 33),
        ] {
            let operation = Operation::ModifiedScroll {
                modifiers,
                x: -700,
                y: 500,
                dx,
                dy,
            };
            assert!(!operation.valid());
            for index in 0..=u8::MAX {
                assert!(operation.action(index).is_none());
                assert!(operation.pointer(index).is_none());
            }
        }
    }
}
