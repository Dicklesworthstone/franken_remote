//! Original local microphone intent. OS permission, host readiness and a Talk
//! gesture are independent prerequisites; none is saved for a future restart.
#![forbid(unsafe_code)]

use crate::macos_clipboard_privacy::Heartbeat;
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

// Portable tests keep the real sampler's fallible signature.
#[cfg_attr(
    not(all(target_os = "macos", feature = "macos-microphone")),
    allow(clippy::unnecessary_wraps)
)]
fn sleep_clock() -> Option<u64> {
    #[cfg(all(target_os = "macos", feature = "macos-microphone"))]
    {
        crate::clock::monotonic_ns()
    }
    #[cfg(not(all(target_os = "macos", feature = "macos-microphone")))]
    {
        Some(0)
    }
}
#[cfg_attr(
    not(all(target_os = "macos", feature = "macos-microphone")),
    allow(clippy::unnecessary_wraps)
)]
fn sleep_offset() -> Option<(u64, u64)> {
    #[cfg(all(target_os = "macos", feature = "macos-microphone"))]
    {
        crate::macos_microphone_capture::sleep_offset()
    }
    #[cfg(not(all(target_os = "macos", feature = "macos-microphone")))]
    {
        Some((0, 0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Allow,
    Talk,
    Mute,
}

#[derive(Clone, Copy)]
struct Sample {
    uptime: Instant,
    sleep_ns: u64,
}
impl Sample {
    fn now() -> Option<Self> {
        Some(Self {
            uptime: Instant::now(),
            sleep_ns: sleep_clock()?,
        })
    }
}
#[derive(Clone, Copy)]
struct Deadline {
    observed: Sample,
    until: Sample,
    permission_until_ns: u64,
}
impl Deadline {
    fn new(observed: Sample, age_ns: u64) -> Option<Self> {
        let remaining = 250_000_000_u64
            .checked_sub(age_ns)
            .filter(|left| *left != 0)?;
        let until = Sample {
            uptime: observed
                .uptime
                .checked_add(Duration::from_nanos(remaining))?,
            sleep_ns: observed.sleep_ns.checked_add(remaining)?,
        };
        Some(Self {
            observed,
            until,
            permission_until_ns: until.sleep_ns.checked_add(59_750_000_000)?,
        })
    }
    fn fresh(self, now: Sample) -> bool {
        now.uptime >= self.observed.uptime
            && now.uptime < self.until.uptime
            && now.sleep_ns >= self.observed.sleep_ns
            && now.sleep_ns < self.until.sleep_ns
    }
}
pub(crate) struct Gesture {
    pub action: Action,
    pub launch_until_ns: u64,
    pub permission_until_ns: u64,
}

#[derive(Default)]
struct State {
    granted: bool,
    ready: bool,
    next: u64,
    talk: Option<u64>,
    pending: Option<Action>,
    pending_at: Option<Deadline>,
    original_sleep: Option<(u64, u64)>,
}

#[derive(Clone, Default)]
pub(crate) struct Intent {
    heartbeat: Heartbeat,
    state: Arc<Mutex<State>>,
}
impl Intent {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| {
            self.heartbeat.stop(false);
            let mut state = error.into_inner();
            state.talk = None;
            state
        })
    }
    pub(crate) fn observe(&self, visible: bool) -> bool {
        if !visible {
            self.stop();
            return false;
        }
        self.check_sleep(true, sleep_offset) && self.heartbeat.observe(true)
    }
    pub(crate) fn live(&self) -> bool {
        self.check_sleep(false, sleep_offset) && self.heartbeat.live()
    }
    fn check_sleep(&self, initialize: bool, sample: impl FnOnce() -> Option<(u64, u64)>) -> bool {
        let mut state = self.lock();
        let current = sample().filter(|(lower, upper)| lower <= upper);
        if initialize && state.original_sleep.is_none() {
            state.original_sleep = current;
        }
        let unchanged = state
            .original_sleep
            .zip(current)
            .is_some_and(|((low, high), (now_low, now_high))| now_low <= high && now_high >= low);
        if !unchanged {
            self.heartbeat.stop(false);
            state.talk = None;
            state.pending = None;
        }
        unchanged
    }
    pub(crate) fn stopped(&self) -> bool {
        self.heartbeat.stopped()
    }
    pub(crate) fn stop(&self) {
        self.heartbeat.stop(false);
        let mut state = self.lock();
        state.talk = None;
        state.pending = None;
        state.pending_at = None;
        state.ready = false;
    }
    pub(crate) fn mute(&self) {
        let mut state = self.lock();
        state.talk = None;
        state.pending = None;
        state.pending_at = None;
    }
    /// Called synchronously from the original native mouse event. Negative
    /// choice fences before returning into `AppKit`, before later session work.
    pub(crate) fn request(&self, action: Action, age_ns: u64) -> bool {
        self.request_with_clock(action, age_ns, Sample::now)
    }
    /// The original `NSEvent` timestamp crosses the native boundary unchanged.
    /// Sample local/sleep clocks BEFORE reading the event clock; a delayed
    /// native visibility query or clock correlation cannot refresh its age.
    pub(crate) fn request_from_event(
        &self,
        action: Action,
        captured_ns: u64,
        event_now: impl FnOnce() -> Option<u64>,
    ) -> bool {
        if action == Action::Mute {
            return self.request(action, 0);
        }
        let Some(observed) = Sample::now() else {
            return false;
        };
        let Some(age) = event_now().and_then(|now| now.checked_sub(captured_ns)) else {
            return false;
        };
        let Some(deadline) = Deadline::new(observed, age) else {
            return false;
        };
        self.admit(action, deadline, Sample::now)
    }
    fn request_with_clock(
        &self,
        action: Action,
        age_ns: u64,
        mut clock: impl FnMut() -> Option<Sample>,
    ) -> bool {
        if action == Action::Mute {
            let mut state = self.lock();
            state.talk = None;
            state.pending = Some(Action::Mute);
            state.pending_at = None;
            return true;
        }
        // Capture the original remaining gesture lifetime BEFORE any state or
        // heartbeat lock. Neither a stalled mutex nor a resumed UI extends it.
        let Some(deadline) = clock().and_then(|now| Deadline::new(now, age_ns)) else {
            return false;
        };
        self.admit(action, deadline, clock)
    }
    fn admit(
        &self,
        action: Action,
        deadline: Deadline,
        mut clock: impl FnMut() -> Option<Sample>,
    ) -> bool {
        if !self.heartbeat.live() {
            return false;
        }
        let mut state = self.lock();
        if state.pending.is_some()
            || state.talk.is_some()
            || !self.heartbeat.live()
            || !clock().is_some_and(|now| deadline.fresh(now))
        {
            return false;
        }
        match action {
            Action::Allow if !state.granted => state.pending = Some(action),
            Action::Talk if state.granted && state.ready => {
                let Some(next) = state.next.checked_add(1) else {
                    self.heartbeat.stop(false);
                    return false;
                };
                state.next = next;
                state.talk = Some(next);
                state.pending = Some(action);
                state.ready = false;
            }
            _ => return false,
        }
        state.pending_at = Some(deadline);
        true
    }
    pub(crate) fn take_request(&self) -> Option<Gesture> {
        if !self.live() {
            self.stop();
            return None;
        }
        let mut state = self.lock();
        if state.pending.is_some() && state.pending != Some(Action::Mute) {
            let fresh = self.heartbeat.live()
                && state
                    .pending_at
                    .is_some_and(|deadline| Sample::now().is_some_and(|now| deadline.fresh(now)));
            if !fresh {
                state.pending = None;
                state.talk = None;
            }
        }
        let deadline = state.pending_at.take();
        state.pending.take().map(|action| Gesture {
            action,
            launch_until_ns: deadline.map_or(0, |deadline| deadline.until.sleep_ns),
            permission_until_ns: deadline.map_or(0, |deadline| deadline.permission_until_ns),
        })
    }
    #[cfg(test)]
    fn take(&self) -> Option<Action> {
        self.take_request().map(|request| request.action)
    }
    /// A completed OS prompt can enable the button, but never begins speech.
    pub(crate) fn grant(&self, granted: bool) {
        let mut state = self.lock();
        state.granted = granted;
        state.talk = None;
        state.pending = None;
        state.pending_at = None;
    }
    pub(crate) fn granted(&self) -> bool {
        self.lock().granted
    }
    pub(crate) fn ready(&self, ready: bool) {
        self.lock().ready = ready;
    }
    pub(crate) fn epoch(&self) -> Option<u64> {
        if !self.live() {
            return None;
        }
        let epoch = {
            let state = self.lock();
            state.granted.then_some(state.talk).flatten()
        };
        self.live().then_some(epoch).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn live() -> Intent {
        let intent = Intent::default();
        assert!(intent.observe(true));
        intent
    }
    #[test]
    fn permission_never_replays_a_talk_or_creates_one() {
        let intent = live();
        intent.ready(true);
        assert!(!intent.request(Action::Talk, 0));
        assert!(intent.request(Action::Allow, 0));
        assert_eq!(intent.take(), Some(Action::Allow));
        intent.grant(true);
        assert_eq!(intent.epoch(), None);
        assert_eq!(intent.take(), None);
        assert!(intent.request(Action::Talk, 0));
        assert_eq!(intent.epoch(), Some(1));
    }
    #[test]
    fn native_mute_fences_original_epoch_before_consuming_action() {
        let intent = live();
        intent.grant(true);
        intent.ready(true);
        assert!(intent.request(Action::Talk, 0));
        assert_eq!(intent.take(), Some(Action::Talk));
        assert_eq!(intent.take(), None);
        assert_eq!(intent.epoch(), Some(1));
        let original = intent.clone();
        assert!(intent.request(Action::Mute, u64::MAX));
        assert_eq!(original.epoch(), None);
        assert_eq!(intent.take(), Some(Action::Mute));
        intent.ready(true);
        assert!(intent.request(Action::Talk, 0));
        assert_eq!(original.epoch(), Some(2));
    }
    #[test]
    fn busy_stale_hidden_and_terminal_gestures_are_consumed_without_restart() {
        let intent = live();
        intent.grant(true);
        assert!(!intent.request(Action::Talk, 0));
        intent.ready(true);
        assert!(!intent.request(Action::Talk, 250_000_000));
        assert!(intent.request(Action::Talk, 100_000_000));
        assert!(!intent.request(Action::Talk, 0));
        assert!(!intent.observe(false));
        assert_eq!(intent.epoch(), None);
        intent.stop();
        intent.grant(true);
        intent.ready(true);
        assert!(!intent.observe(true));
        assert!(!intent.request(Action::Talk, 0));
    }
    #[test]
    fn waiting_for_state_never_refreshes_an_original_positive_gesture() {
        for action in [Action::Allow, Action::Talk] {
            let intent = live();
            if action == Action::Talk {
                intent.grant(true);
                intent.ready(true);
            }
            let start = Instant::now();
            let mut calls = 0;
            assert!(!intent.request_with_clock(action, 100_000_000, || {
                calls += 1;
                let age = if calls == 1 { 0 } else { 150_000_000 };
                Some(Sample {
                    uptime: start + Duration::from_nanos(age),
                    sleep_ns: 1_000_000_000 + age,
                })
            }));
            assert_eq!(calls, 2);
            assert_eq!(intent.take(), None);
            assert_eq!(intent.epoch(), None);
        }
    }
    #[test]
    fn a_failed_deadline_calculation_leaves_no_queued_or_active_consent() {
        let intent = live();
        intent.grant(true);
        intent.ready(true);
        assert!(!intent.request_with_clock(Action::Talk, 0, || Some(Sample {
            uptime: Instant::now(),
            sleep_ns: u64::MAX - 1,
        })));
        assert_eq!(intent.take(), None);
        assert_eq!(intent.epoch(), None);
    }
    #[test]
    fn sleep_interval_never_slides_forward_to_reauthorize_an_old_owner() {
        let intent = Intent::default();
        assert!(intent.check_sleep(true, || Some((10, 12))));
        assert!(intent.check_sleep(false, || Some((11, 13))));
        assert!(!intent.check_sleep(false, || Some((13, 15))));
        assert!(intent.stopped());
        assert!(!intent.observe(true));
        assert_eq!(intent.epoch(), None);
    }
    #[test]
    fn a_stalled_native_visibility_query_preserves_original_event_age() {
        let intent = live();
        intent.grant(true);
        intent.ready(true);
        assert!(!intent.request_from_event(Action::Talk, 1_000_000_000, || Some(1_250_000_000)));
        assert_eq!(intent.take(), None);
        assert_eq!(intent.epoch(), None);
        assert!(intent.request_from_event(Action::Talk, 1_000_000_000, || Some(1_001_000_000)));
        assert_eq!(intent.take(), Some(Action::Talk));
    }
}
