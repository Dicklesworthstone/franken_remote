//! A bounded heartbeat from the original native clipboard's visible Off UI.
//! Expiry, clock regression and stop are terminal; a later paint cannot renew
//! privacy permission. This carries no clipboard data or input authority.
#![forbid(unsafe_code)]

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

pub(crate) const MAX_SILENCE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
struct Sample {
    uptime: Instant,
    sleep_ns: Option<u64>,
}
impl Sample {
    fn now() -> Self {
        // Apple's Instant uses CLOCK_UPTIME_RAW, which pauses during sleep.
        // Keep its existing checks and independently bound the same heartbeat
        // with the already used, sleep-aware CLOCK_MONOTONIC. These samples
        // never translate, refresh or replace an item's publication deadline.
        #[cfg(target_os = "macos")]
        let sleep_ns = crate::clock::monotonic_ns();
        // This module is otherwise compiled only for portable policy tests.
        // Injected samples exercise the sleep clock there without native APIs.
        #[cfg(not(target_os = "macos"))]
        let sleep_ns = Some(0);
        Self {
            uptime: Instant::now(),
            sleep_ns,
        }
    }

    fn expired_since(self, last: Self) -> bool {
        self.uptime
            .checked_duration_since(last.uptime)
            .is_none_or(|age| age >= MAX_SILENCE)
            || self
                .sleep_ns
                .zip(last.sleep_ns)
                .and_then(|(now, last)| now.checked_sub(last))
                .is_none_or(|age| Duration::from_nanos(age) >= MAX_SILENCE)
    }
}

#[derive(Default)]
struct State {
    last: Option<Sample>,
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

    pub(crate) fn progress(&self) -> bool {
        self.progress_with_clock(Sample::now)
    }

    fn progress_with_clock(&self, clock: impl FnOnce() -> Sample) -> bool {
        let mut state = self.lock();
        if state.stopped {
            return false;
        }
        // Sampling must follow lock acquisition. A caller sampled before this
        // mutex could otherwise arrive behind a newer UI turn and falsely
        // retire the original surface as a clock regression.
        let now = clock();
        if now.sleep_ns.is_none() || state.last.is_some_and(|last| now.expired_since(last)) {
            state.stopped = true;
            return false;
        }
        state.last = Some(now);
        true
    }

    pub(crate) fn live(&self) -> bool {
        self.live_with_clock(Sample::now)
    }

    fn live_with_clock(&self, clock: impl FnOnce() -> Sample) -> bool {
        let mut state = self.lock();
        if state.stopped {
            return false;
        }
        let Some(last) = state.last else { return false };
        let now = clock();
        if now.expired_since(last) {
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
    use std::sync::TryLockError;

    const SILENCE_NS: u64 = 250_000_000;

    fn sample(uptime: Instant, sleep_ns: u64) -> Sample {
        Sample {
            uptime,
            sleep_ns: Some(sleep_ns),
        }
    }

    #[test]
    fn an_unmapped_surface_never_permits_access_and_expiry_is_exclusive() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        assert!(!heartbeat.stopped());
        assert!(!heartbeat.live());
        assert!(heartbeat.progress_with_clock(|| sample(start, 0)));
        let before = MAX_SILENCE.checked_sub(Duration::from_nanos(1)).unwrap();
        assert!(heartbeat.live_with_clock(|| sample(start + before, SILENCE_NS - 1)));
        assert!(!heartbeat.live_with_clock(|| sample(start + MAX_SILENCE, SILENCE_NS)));
        assert!(heartbeat.stopped());
        assert!(!heartbeat.progress_with_clock(|| sample(start + MAX_SILENCE, SILENCE_NS)));
        assert!(!heartbeat.live_with_clock(|| sample(start + MAX_SILENCE, SILENCE_NS)));
    }

    #[test]
    fn a_late_ui_turn_cannot_rejuvenate_a_stalled_surface() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        assert!(heartbeat.progress_with_clock(|| sample(start, 0)));
        assert!(!heartbeat.progress_with_clock(|| sample(start + MAX_SILENCE, SILENCE_NS)));
        assert!(!heartbeat.live_with_clock(|| sample(start + MAX_SILENCE, SILENCE_NS)));
    }

    #[test]
    fn off_and_clock_regression_fence_every_clone_without_changing_their_reason() {
        let heartbeat = Heartbeat::default();
        let original = heartbeat.clone();
        let start = Instant::now();
        assert!(heartbeat.progress_with_clock(|| sample(start, 0)));
        heartbeat.stop(true);
        assert!(original.user_off());
        assert!(!original.progress());
        original.stop(false);
        assert!(original.user_off());
        let regressed = Heartbeat::default();
        assert!(regressed.progress_with_clock(|| sample(start + Duration::from_millis(1), 0)));
        assert!(!regressed.live_with_clock(|| sample(start, 0)));
        assert!(!regressed.progress_with_clock(|| sample(start + Duration::from_millis(2), 0)));
        assert!(
            !regressed.user_off(),
            "ordinary retirement is not a user Off gesture"
        );
    }

    #[test]
    fn both_production_clocks_are_sampled_with_the_original_state_locked() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        let clock = || {
            // Moving clock evaluation before lock acquisition fails this
            // deterministic regression, without a scheduling race or sleep.
            assert!(matches!(heartbeat.0.try_lock(), Err(TryLockError::WouldBlock)));
            sample(start, 1)
        };
        assert!(heartbeat.progress_with_clock(clock));
        assert!(heartbeat.live_with_clock(clock));
        assert!(!heartbeat.stopped());
    }

    #[test]
    fn sleep_expires_access_and_a_late_ui_turn_even_when_uptime_did_not_advance() {
        for progress in [false, true] {
            let heartbeat = Heartbeat::default();
            let original = heartbeat.clone();
            let start = Instant::now();
            assert!(heartbeat.progress_with_clock(|| sample(start, 1)));
            assert!(heartbeat.live_with_clock(|| sample(start, SILENCE_NS)));
            let late = || sample(start, SILENCE_NS + 1);
            let allowed = if progress {
                heartbeat.progress_with_clock(late)
            } else {
                heartbeat.live_with_clock(late)
            };
            assert!(!allowed);
            assert!(original.stopped());
            assert!(!original.progress_with_clock(|| sample(start, 1)));
            assert!(!original.user_off());
        }
    }

    #[test]
    fn uptime_expiry_is_retained_when_the_other_clock_did_not_advance() {
        let heartbeat = Heartbeat::default();
        let start = Instant::now();
        assert!(heartbeat.progress_with_clock(|| sample(start, 1)));
        assert!(!heartbeat.live_with_clock(|| sample(start + MAX_SILENCE, 1)));
        assert!(heartbeat.stopped());
    }

    #[test]
    fn a_missing_or_regressed_sleep_clock_is_terminal_on_both_paths() {
        for progress in [false, true] {
            for next in [None, Some(9)] {
                let heartbeat = Heartbeat::default();
                let start = Instant::now();
                assert!(heartbeat.progress_with_clock(|| sample(start, 10)));
                let clock = || Sample {
                    uptime: start,
                    sleep_ns: next,
                };
                let allowed = if progress {
                    heartbeat.progress_with_clock(clock)
                } else {
                    heartbeat.live_with_clock(clock)
                };
                assert!(!allowed);
                assert!(heartbeat.stopped());
                assert!(!heartbeat.progress_with_clock(|| sample(start, 10)));
                assert!(!heartbeat.user_off());
            }
        }
        let unopened = Heartbeat::default();
        assert!(!unopened.progress_with_clock(|| Sample {
            uptime: Instant::now(),
            sleep_ns: None,
        }));
        assert!(unopened.stopped());
    }
}
