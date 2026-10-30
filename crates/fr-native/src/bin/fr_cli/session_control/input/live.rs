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
            Self::KeyTap { usage } if index < 2 => Some(Action::Key {
                key: PhysicalKey::new(usage)?,
                transition: if index == 0 {
                    KeyTransition::Press
                } else {
                    KeyTransition::Release
                },
            }),
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
                .map(|(view, sample)| Candidate { view, sample, at })
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
            && current.is_some_and(|live| live.view == pending.original.view)
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
