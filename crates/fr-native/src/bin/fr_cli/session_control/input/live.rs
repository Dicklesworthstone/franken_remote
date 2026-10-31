//! Dispatch happens only on the original controlled session's synchronous turn.
use super::{Candidate, Shared};
use crate::robot_input::Operation;
use fr_client::input::Action;
use fr_core::input::{DesktopPoint, KeyTransition, PhysicalKey, PointerButton, ScrollUnit};
use frd::session_startup::{ControlledViewer, ControlledViewerError};
use std::time::Instant;

impl Operation {
    fn action(self, index: u8) -> Option<Action<'static>> {
        match self {
            Self::Scroll { x, y, dx, dy } if index == 0 => Some(Action::Scroll {
                position: DesktopPoint { x, y },
                x: dx * fr_core::input_submission::scroll::LINE,
                y: dy * fr_core::input_submission::scroll::LINE,
                unit: ScrollUnit::Lines,
            }),
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
}
impl Shared {
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
                live.view == pending.original.view && live.bounds == pending.original.bounds
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
        let Some(action) = pending.request.operation.action(pending.encoded) else {
            viewer.close();
            state.retire();
            return;
        };
        let binding = viewer.input_result_binding();
        match viewer.action(action) {
            Ok(encoded) => {
                if state.ledger.encoded(id, binding, encoded.sequence).is_err() {
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

    fn key_step(operation: Operation, index: u8) -> (u16, KeyTransition) {
        let Some(Action::Key { key, transition }) = operation.action(index) else {
            panic!("a chord emits only bounded physical key transitions");
        };
        (key.usage(), transition)
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
            assert_eq!(key_step(chord, u8::try_from(index).unwrap()), step);
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
            let (actual, transition) = key_step(chord, u8::try_from(index).unwrap());
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
}
