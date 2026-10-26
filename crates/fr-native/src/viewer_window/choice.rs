//! One fresh local gesture; neither a control grant nor a renewed deadline.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub(super) const MAX_AGE: Duration = Duration::from_millis(250);

#[derive(Default)]
enum State {
    #[default]
    Disabled,
    Offered,
    Chosen(Instant),
    Consumed,
    Unavailable,
}

#[derive(Default)]
pub(super) struct Choice(Mutex<State>);
impl Choice {
    /// Arm once for this fresh viewing owner. No callback can recycle a used choice.
    pub(super) fn enable(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, State::Disabled) {
            return false;
        }
        *state = State::Offered;
        true
    }
    pub(super) fn offered(&self) -> bool {
        matches!(
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            State::Offered
        )
    }
    pub(super) fn unavailable(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, State::Offered) {
            *state = State::Unavailable;
        }
    }
    /// A focus/map change can discard unconsumed intent, never undo a request.
    pub(super) fn invalidate(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, State::Chosen(_)) {
            *state = State::Offered;
        }
    }
    /// Called after the whole native batch, held-key query, and shortcut disable.
    /// Keep the turn's ORIGINAL instant, including all native work in its age.
    pub(super) fn publish(&self, at: Instant) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, State::Offered) && at.elapsed() < MAX_AGE {
            *state = State::Chosen(at);
        }
    }
    /// Stale/not-ready choices are discarded. Only another physical gesture can
    /// try again; focus gain, a fresh frame and clock renewal create no intent.
    pub(super) fn take(&self, ready: bool) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let State::Chosen(at) = *state else {
            return false;
        };
        let accepted = ready && at.elapsed() < MAX_AGE;
        *state = if accepted {
            State::Consumed
        } else {
            State::Offered
        };
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_gesture_with_current_evidence_is_consumed_once() {
        let choice = Choice::default();
        assert!(!choice.take(true));
        assert!(choice.enable());
        assert!(!choice.enable());
        assert!(!choice.take(true));
        choice.publish(Instant::now());
        assert!(choice.take(true));
        assert!(!choice.take(true));
        assert!(!choice.enable());
        choice.invalidate();
        choice.publish(Instant::now());
        assert!(!choice.take(true));
    }

    #[test]
    fn stale_or_not_ready_intent_never_waits_for_later_evidence() {
        let choice = Choice::default();
        assert!(choice.enable());
        choice.publish(Instant::now() - MAX_AGE);
        assert!(!choice.take(true));
        choice.publish(Instant::now());
        assert!(!choice.take(false));
        assert!(!choice.take(true));
        choice.publish(Instant::now());
        choice.invalidate();
        assert!(!choice.take(true));
        choice.publish(Instant::now());
        assert!(choice.take(true));
    }

    #[test]
    fn native_unavailability_never_becomes_a_control_decision() {
        let choice = Choice::default();
        assert!(choice.enable());
        choice.unavailable();
        choice.publish(Instant::now());
        assert!(!choice.take(true));
        assert!(!choice.enable());
    }
    #[test]
    fn duplicate_publication_never_refreshes_a_latched_decision() {
        let original = Instant::now();
        let choice = Choice(Mutex::new(State::Chosen(original)));
        choice.publish(Instant::now());
        {
            let mut state = choice.0.lock().unwrap();
            assert!(matches!(*state, State::Chosen(at) if at == original));
            // The exact same retained slot after its original deadline.
            *state = State::Chosen(Instant::now() - MAX_AGE);
        }
        assert!(!choice.take(true));
        assert!(!choice.take(true));
        choice.publish(Instant::now());
        assert!(choice.take(true));
    }
}
