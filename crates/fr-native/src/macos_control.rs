//! Local control intent for the macOS shell. No lease, ticket or input action
//! lives here. The command-line choice is consumed once for the invocation;
//! every subsequent request needs a new, current F8 press and release.
#![forbid(unsafe_code)]

use std::time::Duration;

const F8: u16 = 0x41;
const ESCAPE: u16 = 0x29;
const MAX_AGE: Duration = Duration::from_millis(250);
const SHIFT: u64 = 1 << 17;
const CONTROL: u64 = 1 << 18;
const OPTION: u64 = 1 << 19;
const COMMAND: u64 = 1 << 20;

/// A terminal local escape needs no usable picture, clock or remote grant.
/// The shell checks it before any readiness gate can discard the event batch.
pub(crate) const fn emergency_escape(usage: u16, pressed: bool, modifiers: u64) -> bool {
    usage == ESCAPE && pressed && modifiers & (SHIFT | CONTROL | OPTION) == SHIFT | CONTROL | OPTION
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Key {
    pub(crate) usage: u16,
    pub(crate) pressed: bool,
    pub(crate) repeat: bool,
    /// Original `NSEvent.timestamp`, in the native monotonic uptime domain.
    pub(crate) captured: Option<Duration>,
    pub(crate) modifiers: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    #[default]
    Remote,
    Local,
    Escape,
}

pub(crate) struct Intent {
    initial: bool,
    requested: bool,
    offered: Option<Duration>,
    held: [bool; 256],
    buttons: u32,
    f8_local: bool,
    pressed: Option<Duration>,
    chosen: Option<Duration>,
}

impl Intent {
    pub(crate) const fn new(initial: bool) -> Self {
        Self {
            initial,
            requested: false,
            offered: None,
            held: [false; 256],
            buttons: 0,
            f8_local: false,
            pressed: None,
            chosen: None,
        }
    }

    /// A new window never inherits a gesture, held state or spent request.
    /// Only command-line consent that was never used can survive a failed dial.
    pub(crate) fn begin_attempt(&mut self) {
        *self = Self::new(self.initial);
    }

    pub(crate) const fn requested(&self) -> bool {
        self.requested
    }

    pub(crate) const fn needs_explicit_request(&self) -> bool {
        !self.initial
    }

    /// Focus, geometry, visibility or evidence loss discards an unfinished
    /// gesture. Re-entering viewing creates a new timestamp boundary.
    pub(crate) fn invalidate(&mut self) {
        self.offered = None;
        self.pressed = None;
        self.chosen = None;
    }

    /// Releases may happen outside an unfocused window. Forget local tracking
    /// without adopting a held key; a returning repeat or unmatched release
    /// still cannot complete a new gesture.
    pub(crate) fn suspend(&mut self) {
        self.invalidate();
        self.held = [false; 256];
        self.buttons = 0;
        self.f8_local = false;
    }

    /// Consume either the unused invocation choice or one current local F8
    /// choice, before the caller submits `RequestControl`. Missing readiness
    /// discards the gesture instead of saving it for a future picture.
    pub(crate) fn take_request(&mut self, ready: bool, now: Option<Duration>) -> bool {
        if self.requested {
            self.invalidate();
            return false;
        }
        let chosen = self.chosen.take();
        let Some(now) = now.filter(|_| ready) else {
            self.invalidate();
            return false;
        };
        if self.initial || chosen.is_some_and(|at| fresh(at, now)) {
            self.initial = false;
            self.requested = true;
            self.invalidate();
            return true;
        }
        self.offered.get_or_insert(now);
        false
    }

    pub(crate) fn button(&mut self, button: u8, pressed: bool) {
        let Some(bit) = 1_u32.checked_shl(u32::from(button)) else {
            self.invalidate();
            return;
        };
        self.pressed = None;
        self.chosen = None;
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
    }

    /// Inspect the whole batch before forwarding any input. Emergency release
    /// needs no remote capability and wins over every ordinary key in the batch.
    pub(crate) fn key(&mut self, key: Key, now: Option<Duration>) -> Route {
        let usage = usize::from(key.usage);
        if usage >= self.held.len() {
            self.invalidate();
            return Route::Remote;
        }
        let was_held = std::mem::replace(&mut self.held[usage], key.pressed);
        if emergency_escape(key.usage, key.pressed, key.modifiers) {
            self.invalidate();
            return Route::Escape;
        }
        if usage != usize::from(F8) {
            self.pressed = None;
            self.chosen = None;
            return Route::Remote;
        }
        if key.pressed {
            // A repeat, duplicate or key held through a mode change cannot
            // acquire a new timestamp or turn into a fresh local choice.
            if was_held || key.repeat {
                self.pressed = None;
                self.chosen = None;
                return if self.f8_local {
                    Route::Local
                } else {
                    Route::Remote
                };
            }
            let captured = key.captured.filter(|at| {
                self.offered.is_some_and(|offered| *at >= offered)
                    && now.is_some_and(|now| fresh(*at, now))
                    && key.modifiers & (SHIFT | CONTROL | OPTION | COMMAND) == 0
                    && self.buttons == 0
                    && !self
                        .held
                        .iter()
                        .enumerate()
                        .any(|(i, held)| i != usize::from(F8) && *held)
                    && !self.requested
            });
            self.f8_local = self.offered.is_some() && !self.requested;
            self.pressed = captured;
            self.chosen = None;
            return if self.f8_local {
                Route::Local
            } else {
                Route::Remote
            };
        }
        let local = std::mem::take(&mut self.f8_local);
        let press = self.pressed.take();
        self.chosen = None;
        if local && was_held && !self.requested {
            self.chosen = press.filter(|press| {
                key.captured.is_some_and(|up| up >= *press)
                    && key
                        .captured
                        .zip(now)
                        .is_some_and(|(up, now)| fresh(up, now))
                    && now.is_some_and(|now| fresh(*press, now))
                    && self.offered.is_some_and(|offered| *press >= offered)
                    && self.buttons == 0
                    && !self.held.iter().any(|held| *held)
                    && key.modifiers & (SHIFT | CONTROL | OPTION | COMMAND) == 0
            });
        }
        if local { Route::Local } else { Route::Remote }
    }
}

fn fresh(captured: Duration, now: Duration) -> bool {
    now.checked_sub(captured).is_some_and(|age| age < MAX_AGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps)] // Models an available optional native clock.
    fn at(ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(ms))
    }
    fn key(usage: u16, pressed: bool, ms: u64) -> Key {
        Key {
            usage,
            pressed,
            repeat: false,
            captured: at(ms),
            modifiers: 0,
        }
    }
    fn recovered() -> Intent {
        let mut intent = Intent::new(true);
        assert!(intent.take_request(true, at(1)));
        intent.begin_attempt();
        assert!(!intent.take_request(true, at(10)));
        intent
    }
    fn tap(intent: &mut Intent, down: u64, up: u64, now: u64) {
        assert_eq!(intent.key(key(F8, true, down), at(now)), Route::Local);
        assert_eq!(intent.key(key(F8, false, up), at(now)), Route::Local);
    }

    #[test]
    fn initial_consent_survives_failed_dials_but_is_consumed_before_any_retry() {
        let mut intent = Intent::new(true);
        intent.begin_attempt();
        assert!(!intent.take_request(false, at(1)));
        intent.begin_attempt();
        assert!(intent.take_request(true, at(2)));
        assert!(intent.requested());
        assert!(!intent.take_request(true, at(3)));
        for attempt in 3..=32 {
            intent.begin_attempt();
            assert!(intent.needs_explicit_request());
            assert!(!intent.take_request(true, at(attempt)));
        }
    }

    #[test]
    fn recovered_viewing_needs_a_new_complete_gesture_and_consumes_it_once() {
        let mut intent = recovered();
        assert!(!intent.take_request(true, at(11)));
        tap(&mut intent, 12, 13, 14);
        assert!(intent.take_request(true, at(15)));
        assert!(!intent.take_request(true, at(16)));
        intent.begin_attempt();
        assert!(!intent.take_request(true, at(17)));
        assert_eq!(intent.key(key(F8, false, 18), at(19)), Route::Remote);
        assert!(!intent.take_request(true, at(20)));
    }

    #[test]
    fn stale_future_and_preoffer_events_never_gain_fresh_consent() {
        for (down, up, now) in [(1, 11, 12), (11, 12, 261), (20, 21, 19), (11, 10, 12)] {
            let mut intent = recovered();
            tap(&mut intent, down, up, now);
            assert!(!intent.take_request(true, at(now)));
        }
        let mut intent = recovered();
        tap(&mut intent, 11, 12, 13);
        assert!(
            !intent.take_request(true, at(261)),
            "queued intent keeps original press age"
        );
        assert!(!intent.take_request(true, at(262)));
    }

    #[test]
    fn held_repeated_modified_or_interrupted_gestures_do_not_request_control() {
        for repeat in [false, true] {
            let mut intent = recovered();
            intent.key(key(F8, true, 11), at(12));
            let mut duplicate = key(F8, true, 13);
            duplicate.repeat = repeat;
            intent.key(duplicate, at(14));
            intent.key(key(F8, false, 15), at(16));
            assert!(!intent.take_request(true, at(17)));
        }
        for modifiers in [SHIFT, CONTROL, OPTION, COMMAND] {
            let mut intent = recovered();
            let mut down = key(F8, true, 11);
            down.modifiers = modifiers;
            intent.key(down, at(12));
            intent.key(key(F8, false, 13), at(14));
            assert!(!intent.take_request(true, at(15)));
        }
        let mut intent = recovered();
        intent.key(key(0x04, true, 11), at(12));
        tap(&mut intent, 13, 14, 15);
        intent.key(key(0x04, false, 16), at(17));
        assert!(!intent.take_request(true, at(18)));
        tap(&mut intent, 19, 20, 21);
        intent.key(key(0x04, true, 22), at(23));
        intent.key(key(0x04, false, 24), at(25));
        assert!(
            !intent.take_request(true, at(26)),
            "later batch input invalidates choice"
        );
    }

    #[test]
    fn held_before_offer_or_at_focus_loss_requires_release_then_a_new_press() {
        let mut intent = Intent::new(false);
        assert_eq!(intent.key(key(F8, true, 1), at(2)), Route::Remote);
        assert!(!intent.take_request(true, at(3)));
        assert_eq!(intent.key(key(F8, false, 4), at(5)), Route::Remote);
        assert!(!intent.take_request(true, at(6)));
        intent.key(key(F8, true, 7), at(8));
        intent.invalidate();
        assert!(!intent.take_request(true, at(9)));
        assert_eq!(intent.key(key(F8, false, 10), at(11)), Route::Local);
        assert!(!intent.take_request(true, at(12)));
        tap(&mut intent, 13, 14, 15);
        assert!(intent.take_request(true, at(16)));
    }

    #[test]
    fn missing_evidence_buttons_and_invalid_clock_discard_intent() {
        let mut intent = recovered();
        tap(&mut intent, 11, 12, 13);
        assert!(!intent.take_request(false, at(14)));
        assert!(!intent.take_request(true, at(15)));
        intent.button(0, true);
        tap(&mut intent, 16, 17, 18);
        intent.button(0, false);
        assert!(!intent.take_request(true, at(19)));
        tap(&mut intent, 20, 21, 22);
        assert!(!intent.take_request(true, None));
        assert!(!intent.take_request(true, at(23)));
        let mut down = key(F8, true, 24);
        down.captured = None;
        intent.key(down, at(25));
        intent.key(key(F8, false, 26), at(27));
        assert!(!intent.take_request(true, at(28)));
    }

    #[test]
    fn a_focus_lapse_forgets_lost_releases_without_adopting_held_keys() {
        let mut intent = recovered();
        intent.key(key(0x04, true, 11), at(12));
        intent.key(key(F8, true, 13), at(14));
        intent.suspend();
        assert!(!intent.take_request(true, at(15)));
        let mut repeat = key(F8, true, 16);
        repeat.repeat = true;
        intent.key(repeat, at(17));
        intent.key(key(F8, false, 18), at(19));
        assert!(!intent.take_request(true, at(20)));
        tap(&mut intent, 21, 22, 23);
        assert!(intent.take_request(true, at(24)));
    }

    #[test]
    fn emergency_escape_is_local_in_viewing_requesting_and_controlled_modes() {
        for initial in [true, false] {
            let mut intent = Intent::new(initial);
            let mut escape = key(ESCAPE, true, 1);
            escape.modifiers = SHIFT | CONTROL | OPTION;
            assert_eq!(intent.key(escape, at(2)), Route::Escape);
        }
        for missing in [SHIFT, CONTROL, OPTION] {
            let mut intent = recovered();
            let mut escape = key(ESCAPE, true, 11);
            escape.modifiers = (SHIFT | CONTROL | OPTION) & !missing;
            assert_eq!(intent.key(escape, at(12)), Route::Remote);
        }
    }
}
