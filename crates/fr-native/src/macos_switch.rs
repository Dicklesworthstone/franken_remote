//! One explicit native display-change or control-release decision on the original window.
//! This policy owns no connection, display handle, input grant or native task.
#![forbid(unsafe_code)]

use std::time::Duration;

const CLICK_AGE: Duration = Duration::from_millis(250);
const CLOSE_TURN: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Disabled,
    Viewing,
    Controlled,
}

/// Local chrome actions only. These values match the native action latch,
/// never a protocol message or a reusable authority token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Action {
    ChangeDisplay = 1,
    StopControl = 2,
}
impl Action {
    pub(crate) const fn from_native(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::ChangeDisplay),
            2 => Some(Self::StopControl),
            _ => None,
        }
    }

    const fn allowed(self, mode: Mode) -> bool {
        matches!(mode, Mode::Controlled)
            || matches!((self, mode), (Self::ChangeDisplay, Mode::Viewing))
    }
}

#[derive(Debug, Clone, Copy)]
struct Choice {
    action: Action,
    captured: Duration,
}

#[derive(Debug, Default)]
pub(crate) struct Intent {
    mode: Mode,
    offered: Option<Duration>,
    pending: Option<Choice>,
    closing: bool,
    consumed: bool,
    cancelled: bool,
}
impl Intent {
    pub(crate) fn begin_attempt(&mut self) {
        *self = Self::default();
    }

    /// A changed mode establishes a new native timestamp boundary. Requesting
    /// control is Disabled; a click captured before the later grant cannot act.
    pub(crate) fn set_mode(&mut self, mode: Mode, now: Option<Duration>) {
        if self.pending.is_some() || self.cancelled || self.consumed {
            return;
        }
        if self.mode != mode || self.offered.is_none() {
            self.mode = mode;
            self.offered = now.filter(|_| mode != Mode::Disabled);
        }
    }

    pub(crate) fn enabled(&self, action: Action) -> bool {
        action.allowed(self.mode)
            && self.offered.is_some()
            && self.pending.is_none()
            && !self.consumed
            && !self.cancelled
    }

    /// Consume only a current native button action in its original mode.
    pub(crate) fn request(
        &mut self,
        action: Action,
        captured: Duration,
        now: Option<Duration>,
    ) -> bool {
        if !self.enabled(action)
            || self.offered.is_none_or(|offered| captured < offered)
            || now.is_none_or(|now| now.checked_sub(captured).is_none_or(|age| age >= CLICK_AGE))
        {
            return false;
        }
        self.pending = Some(Choice { action, captured });
        true
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some() && !self.cancelled
    }

    /// A callback's later native sample can invalidate a choice accepted by
    /// the outer pump. Never carry that choice through a hidden or unfocused
    /// window, a native read failure, or a pending control request.
    pub(crate) fn reject_ineligible(&mut self, mode: Mode) -> bool {
        if self
            .pending
            .is_some_and(|choice| !choice.action.allowed(mode))
        {
            self.cancel();
            true
        } else {
            false
        }
    }

    /// Park input only until the original service can begin its close. A
    /// stalled service gets an immediate terminal stop, never a new connection.
    pub(crate) fn expired(&self, now: Option<Duration>) -> bool {
        !self.closing
            && self.pending.is_some_and(|choice| {
                now.and_then(|now| now.checked_sub(choice.captured))
                    .is_none_or(|age| age >= CLOSE_TURN)
            })
    }

    /// Called in the original viewer's UI callback immediately before Break.
    /// Its own bounded closing exchange now owns timing and the input fence.
    pub(crate) fn begin_close(&mut self) -> bool {
        if !self.pending() || self.closing {
            return false;
        }
        self.closing = true;
        true
    }

    /// Window close, escape, focus loss and native failure win permanently.
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
        self.pending = None;
        self.offered = None;
    }

    /// The caller first verifies successful service, original close evidence
    /// and actual native cleanup. This consumes only the one local decision.
    pub(crate) fn consume(&mut self) -> Option<Action> {
        if !self.closing || self.cancelled || self.consumed {
            return None;
        }
        let choice = self.pending.take()?;
        self.consumed = true;
        self.offered = None;
        Some(choice.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{ChangeDisplay, StopControl};

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn a_current_native_decision_is_consumed_once() {
        let mut intent = Intent::default();
        assert!(!intent.request(ChangeDisplay, at(10), Some(at(11))));
        intent.set_mode(Mode::Viewing, Some(at(10)));
        assert!(intent.request(ChangeDisplay, at(11), Some(at(12))));
        assert!(intent.pending());
        assert!(!intent.enabled(ChangeDisplay));
        assert!(!intent.request(ChangeDisplay, at(12), Some(at(13))));
        assert_eq!(intent.consume(), None);
        assert!(intent.begin_close());
        assert_eq!(intent.consume(), Some(ChangeDisplay));
        assert_eq!(intent.consume(), None);
        intent.set_mode(Mode::Controlled, Some(at(14)));
        assert!(!intent.enabled(ChangeDisplay));
    }

    #[test]
    fn stale_future_missing_clock_and_preoffer_clicks_cannot_reopen() {
        for action in [ChangeDisplay, StopControl] {
            for (captured, now) in [(9, Some(11)), (11, Some(261)), (12, Some(11)), (11, None)] {
                let mut intent = Intent::default();
                intent.set_mode(Mode::Controlled, Some(at(10)));
                assert!(!intent.request(action, at(captured), now.map(at)));
                assert!(!intent.pending());
            }
        }
    }

    #[test]
    fn requesting_and_a_later_grant_never_adopt_an_earlier_click() {
        let mut intent = Intent::default();
        intent.set_mode(Mode::Viewing, Some(at(1)));
        intent.set_mode(Mode::Disabled, Some(at(10)));
        assert!(!intent.request(ChangeDisplay, at(11), Some(at(12))));
        intent.set_mode(Mode::Controlled, Some(at(20)));
        assert!(!intent.request(ChangeDisplay, at(19), Some(at(21))));
        assert!(intent.request(ChangeDisplay, at(21), Some(at(22))));
    }

    #[test]
    fn local_stop_wins_and_new_attempt_has_no_saved_choice() {
        let mut intent = Intent::default();
        intent.set_mode(Mode::Controlled, Some(at(1)));
        assert!(intent.request(ChangeDisplay, at(2), Some(at(3))));
        intent.cancel();
        assert_eq!(intent.consume(), None);
        intent.set_mode(Mode::Viewing, Some(at(4)));
        assert!(!intent.enabled(ChangeDisplay));
        intent.begin_attempt();
        assert!(!intent.pending());
        assert_eq!(intent.consume(), None);
        intent.set_mode(Mode::Viewing, Some(at(5)));
        assert!(intent.request(ChangeDisplay, at(6), Some(at(7))));
    }

    #[test]
    fn a_later_ineligible_callback_sample_retires_the_accepted_choice() {
        for mode in [Mode::Viewing, Mode::Controlled] {
            let mut intent = Intent::default();
            intent.set_mode(mode, Some(at(1)));
            assert!(!intent.reject_ineligible(Mode::Disabled));
            assert!(intent.request(ChangeDisplay, at(2), Some(at(3))));
            assert!(!intent.reject_ineligible(mode));
            // Mode publication cannot reinterpret an already accepted click;
            // the callback must explicitly reject it using its later sample.
            intent.set_mode(Mode::Disabled, Some(at(4)));
            assert!(intent.pending());
            assert!(intent.reject_ineligible(Mode::Disabled));
            assert!(!intent.begin_close());
            assert_eq!(intent.consume(), None);
            intent.set_mode(mode, Some(at(5)));
            assert!(!intent.request(ChangeDisplay, at(6), Some(at(7))));
        }
    }

    #[test]
    fn a_parked_producer_has_one_original_close_deadline() {
        let mut intent = Intent::default();
        assert!(!intent.expired(None));
        intent.set_mode(Mode::Viewing, Some(at(1)));
        assert!(intent.request(ChangeDisplay, at(2), Some(at(3))));
        assert!(!intent.expired(Some(at(501))));
        assert!(intent.expired(Some(at(502))));
        assert!(intent.expired(Some(at(1))));
        assert!(intent.expired(None));
        intent.set_mode(Mode::Controlled, Some(at(503)));
        assert!(intent.expired(Some(at(504))));
        assert!(intent.begin_close());
        assert!(!intent.expired(Some(at(1000))));
    }

    #[test]
    fn stop_control_requires_the_current_grant_and_consumes_only_that_choice() {
        let mut intent = Intent::default();
        for mode in [Mode::Disabled, Mode::Viewing] {
            intent.set_mode(mode, Some(at(10)));
            assert!(!intent.enabled(StopControl));
            assert!(!intent.request(StopControl, at(11), Some(at(12))));
        }
        intent.set_mode(Mode::Controlled, Some(at(20)));
        assert!(intent.enabled(StopControl));
        assert!(!intent.request(StopControl, at(19), Some(at(21))));
        assert!(intent.request(StopControl, at(21), Some(at(22))));
        assert!(!intent.request(ChangeDisplay, at(22), Some(at(23))));
        assert!(!intent.request(StopControl, at(22), Some(at(23))));
        assert!(!intent.reject_ineligible(Mode::Controlled));
        assert_eq!(intent.consume(), None);
        assert!(intent.begin_close());
        assert!(!intent.begin_close());
        assert_eq!(intent.consume(), Some(StopControl));
        assert_eq!(intent.consume(), None);
        intent.begin_attempt();
        intent.set_mode(Mode::Viewing, Some(at(30)));
        assert!(!intent.pending());
        assert!(!intent.enabled(StopControl));
        assert!(intent.enabled(ChangeDisplay));
    }

    #[test]
    fn a_lost_grant_or_terminal_event_cannot_turn_stop_control_into_a_reopen() {
        for mode in [Mode::Disabled, Mode::Viewing] {
            let mut intent = Intent::default();
            intent.set_mode(Mode::Controlled, Some(at(1)));
            assert!(intent.request(StopControl, at(2), Some(at(3))));
            assert!(intent.reject_ineligible(mode));
            assert!(!intent.begin_close());
            assert_eq!(intent.consume(), None);
        }
        let mut intent = Intent::default();
        intent.set_mode(Mode::Controlled, Some(at(1)));
        assert!(intent.request(StopControl, at(2), Some(at(3))));
        assert!(intent.begin_close());
        intent.cancel();
        assert_eq!(intent.consume(), None);
    }

    #[test]
    fn stop_control_cannot_extend_the_original_close_start_deadline() {
        let mut intent = Intent::default();
        intent.set_mode(Mode::Controlled, Some(at(1)));
        assert!(intent.request(StopControl, at(2), Some(at(3))));
        assert!(!intent.expired(Some(at(501))));
        assert!(intent.expired(Some(at(502))));
        assert!(intent.expired(None));
        assert!(!intent.request(StopControl, at(502), Some(at(503))));
        assert!(intent.expired(Some(at(503))));
    }

    #[test]
    fn the_native_action_namespace_cannot_invent_a_reopen_choice() {
        assert_eq!(Action::from_native(1), Some(ChangeDisplay));
        assert_eq!(Action::from_native(2), Some(StopControl));
        for raw in [0, 3, u32::MAX] {
            assert_eq!(Action::from_native(raw), None);
        }
    }
}
