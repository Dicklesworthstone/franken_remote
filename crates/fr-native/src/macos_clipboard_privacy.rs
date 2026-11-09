//! A bounded heartbeat from the original native clipboard's visible Off UI.
//! Expiry, clock regression and stop are terminal; a later paint cannot renew
//! privacy permission. This carries no clipboard data or input authority.
#![forbid(unsafe_code)]

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

pub(crate) const MAX_SILENCE: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    last: Option<Instant>,
    stopped: bool,
    user_off: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Heartbeat(Arc<Mutex<State>>);

impl Heartbeat {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.stopped = true;
            state
        })
    }

    pub(crate) fn progress(&self, now: Instant) -> bool {
        let mut state = self.lock();
        if state.stopped {
            return false;
        }
        if state.last.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_none_or(|age| age >= MAX_SILENCE)
        }) {
            state.stopped = true;
            return false;
        }
        state.last = Some(now);
        true
    }

    pub(crate) fn live(&self, now: Instant) -> bool {
        let mut state = self.lock();
        if state.stopped {
            return false;
        }
        let Some(last) = state.last else { return false };
        if now
            .checked_duration_since(last)
            .is_none_or(|age| age >= MAX_SILENCE)
        {
            state.stopped = true;
            return false;
        }
        true
    }

    pub(crate) fn stop(&self, user_off: bool) {
        let mut state = self.lock();
        state.stopped = true;
        state.user_off |= user_off;
    }

    pub(crate) fn user_off(&self) -> bool {
        self.lock().user_off
    }
    pub(crate) fn stopped(&self) -> bool {
        self.lock().stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmapped_surface_never_permits_access_and_expiry_is_exclusive() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        assert!(!heartbeat.stopped());
        assert!(!heartbeat.live(start));
        assert!(heartbeat.progress(start));
        assert!(heartbeat.live(start + MAX_SILENCE.checked_sub(Duration::from_nanos(1)).unwrap()));
        assert!(!heartbeat.live(start + MAX_SILENCE));
        assert!(heartbeat.stopped());
        assert!(!heartbeat.progress(start + MAX_SILENCE));
        assert!(!heartbeat.live(start + MAX_SILENCE));
    }

    #[test]
    fn a_late_ui_turn_cannot_rejuvenate_a_stalled_surface() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        assert!(heartbeat.progress(start));
        assert!(!heartbeat.progress(start + MAX_SILENCE));
        assert!(!heartbeat.live(start + MAX_SILENCE));
    }

    #[test]
    fn off_and_clock_regression_fence_every_clone_without_changing_their_reason() {
        let heartbeat = Heartbeat::default();
        let original = heartbeat.clone();
        let start = Instant::now();
        assert!(heartbeat.progress(start));
        heartbeat.stop(true);
        assert!(original.user_off());
        assert!(!original.progress(start));
        original.stop(false);
        assert!(original.user_off());
        let regressed = Heartbeat::default();
        assert!(regressed.progress(start + Duration::from_millis(1)));
        assert!(!regressed.live(start));
        assert!(!regressed.progress(start + Duration::from_millis(2)));
        assert!(
            !regressed.user_off(),
            "ordinary retirement is not a user Off gesture"
        );
    }
}
