//! One explicit native display-change decision on the original window.
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

#[derive(Debug, Default)]
pub(crate) struct Intent {
    mode: Mode,
    offered: Option<Duration>,
    pending: Option<Duration>,
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

    pub(crate) fn enabled(&self) -> bool {
        self.mode != Mode::Disabled
            && self.offered.is_some()
            && self.pending.is_none()
            && !self.consumed
            && !self.cancelled
    }

    /// Consume only a current native button action in its original mode.
    pub(crate) fn request(&mut self, captured: Duration, now: Option<Duration>) -> bool {
        if !self.enabled()
            || self.offered.is_none_or(|offered| captured < offered)
            || now.is_none_or(|now| now.checked_sub(captured).is_none_or(|age| age >= CLICK_AGE))
        {
            return false;
        }
        self.pending = Some(captured);
        true
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some() && !self.cancelled
    }

    /// A callback's later native sample can invalidate a choice accepted by
    /// the outer pump. Never carry that choice through a hidden or unfocused
    /// window, a native read failure, or a pending control request.
    pub(crate) fn reject_ineligible(&mut self, eligible: bool) -> bool {
        if self.pending() && !eligible {
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
            && self.pending.is_some_and(|captured| {
                now.and_then(|now| now.checked_sub(captured))
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
    pub(crate) fn consume(&mut self) -> bool {
        if !self.closing || self.cancelled || self.consumed || self.pending.take().is_none() {
            return false;
        }
        self.consumed = true;
        self.offered = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn a_current_native_decision_is_consumed_once() {
        let mut intent = Intent::default();
        assert!(!intent.request(at(10), Some(at(11))));
        intent.set_mode(Mode::Viewing, Some(at(10)));
        assert!(intent.request(at(11), Some(at(12))));
        assert!(intent.pending());
        assert!(!intent.enabled());
        assert!(!intent.request(at(12), Some(at(13))));
        assert!(!intent.consume());
        assert!(intent.begin_close());
        assert!(intent.consume());
        assert!(!intent.consume());
        intent.set_mode(Mode::Controlled, Some(at(14)));
        assert!(!intent.enabled());
    }

    #[test]
    fn stale_future_missing_clock_and_preoffer_clicks_cannot_reopen() {
        for (captured, now) in [(9, Some(11)), (11, Some(261)), (12, Some(11)), (11, None)] {
            let mut intent = Intent::default();
            intent.set_mode(Mode::Viewing, Some(at(10)));
            assert!(!intent.request(at(captured), now.map(at)));
            assert!(!intent.pending());
        }
    }

    #[test]
    fn requesting_and_a_later_grant_never_adopt_an_earlier_click() {
        let mut intent = Intent::default();
        intent.set_mode(Mode::Viewing, Some(at(1)));
        intent.set_mode(Mode::Disabled, Some(at(10)));
        assert!(!intent.request(at(11), Some(at(12))));
        intent.set_mode(Mode::Controlled, Some(at(20)));
        assert!(!intent.request(at(19), Some(at(21))));
        assert!(intent.request(at(21), Some(at(22))));
    }

    #[test]
    fn local_stop_wins_and_new_attempt_has_no_saved_choice() {
        let mut intent = Intent::default();
        intent.set_mode(Mode::Controlled, Some(at(1)));
        assert!(intent.request(at(2), Some(at(3))));
        intent.cancel();
        assert!(!intent.consume());
        intent.set_mode(Mode::Viewing, Some(at(4)));
        assert!(!intent.enabled());
        intent.begin_attempt();
        assert!(!intent.pending());
        assert!(!intent.consume());
        intent.set_mode(Mode::Viewing, Some(at(5)));
        assert!(intent.request(at(6), Some(at(7))));
    }

    #[test]
    fn a_later_ineligible_callback_sample_retires_the_accepted_choice() {
        for mode in [Mode::Viewing, Mode::Controlled] {
            let mut intent = Intent::default();
            intent.set_mode(mode, Some(at(1)));
            assert!(!intent.reject_ineligible(false));
            assert!(intent.request(at(2), Some(at(3))));
            assert!(!intent.reject_ineligible(true));
            // Mode publication cannot reinterpret an already accepted click;
            // the callback must explicitly reject it using its later sample.
            intent.set_mode(Mode::Disabled, Some(at(4)));
            assert!(intent.pending());
            assert!(intent.reject_ineligible(false));
            assert!(!intent.begin_close());
            assert!(!intent.consume());
            intent.set_mode(mode, Some(at(5)));
            assert!(!intent.request(at(6), Some(at(7))));
        }
    }

    #[test]
    fn a_parked_producer_has_one_original_close_deadline() {
        let mut intent = Intent::default();
        assert!(!intent.expired(None));
        intent.set_mode(Mode::Viewing, Some(at(1)));
        assert!(intent.request(at(2), Some(at(3))));
        assert!(!intent.expired(Some(at(501))));
        assert!(intent.expired(Some(at(502))));
        assert!(intent.expired(Some(at(1))));
        assert!(intent.expired(None));
        intent.set_mode(Mode::Controlled, Some(at(503)));
        assert!(intent.expired(Some(at(504))));
        assert!(intent.begin_close());
        assert!(!intent.expired(Some(at(1000))));
    }
}
