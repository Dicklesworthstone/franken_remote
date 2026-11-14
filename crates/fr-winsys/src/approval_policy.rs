//! Bounds and local gesture custody for the Windows consent surface. This
//! policy creates no approval capability and knows no peer or session IDs.
#![forbid(unsafe_code)]

pub(super) const OPEN_NS: u64 = 3_000_000_000;
pub(super) const TOTAL_NS: u64 = 30_000_000_000;
const QUEUE_MS: u64 = 250;
const CLICK_MS: u64 = 2_000;
// GetMessageTime uses the coarse uptime clock. Do not accept a message from
// the tick in which the surface became ready, including its rounding margin.
const TICK_MARGIN_MS: u64 = 16;

pub(super) fn valid_details(details: &str) -> bool {
    !details.is_empty()
        && details.len() <= 1024
        && details.lines().count() <= 14
        && details.lines().all(|line| line.len() <= 78)
        && details
            .bytes()
            .all(|byte| byte == b'\n' || (32..=126).contains(&byte))
}

pub(super) struct Budget {
    started: u64,
    last: u64,
    failed: bool,
}

impl Budget {
    pub(super) const fn new(started: u64) -> Self {
        Self {
            started,
            last: started,
            failed: false,
        }
    }

    /// Neither making the window visible nor receiving an event renews time.
    pub(super) fn check(&mut self, now: u64, opening: bool) -> bool {
        let limit = if opening { OPEN_NS } else { TOTAL_NS };
        self.failed |= now < self.last
            || self
                .started
                .checked_add(limit)
                .is_none_or(|until| now >= until);
        self.last = now;
        !self.failed
    }
}

/// One original visible surface and one original mouse press. Provenance is
/// supplied by `GetCurrentInputMessageSource` INSIDE the native input callback.
/// Unknown, injected, system or non-mouse input clears a partial gesture.
#[derive(Default)]
pub(super) struct Choice {
    shown: Option<u64>,
    pressed: Option<u64>,
    answer: Option<bool>,
    allowed_at: Option<u64>,
}

impl Choice {
    pub(super) fn show(&mut self, now: u64) {
        if self.shown.is_none() && self.answer.is_none() {
            self.shown = Some(now);
        }
    }

    pub(super) fn invalidate_gesture(&mut self) {
        self.pressed = None;
    }

    /// A move, size or focus transition cannot restore a partially completed
    /// click or a delivered yes. Before a choice, a fresh click remains possible.
    pub(super) fn context_changed(&mut self) {
        self.pressed = None;
        if self.answer == Some(true) {
            self.deny();
        }
    }

    pub(super) fn deny(&mut self) {
        self.pressed = None;
        // A nested close/cancellation beats an unconsumed native yes.
        self.answer = Some(false);
        self.allowed_at = None;
    }

    pub(super) const fn answer(&self) -> Option<bool> {
        self.answer
    }

    /// Native destruction cannot make a delivered yes younger. Its source
    /// release time, including the clock's rounding margin, remains the cap.
    pub(super) fn current_allow(&self, now: u64) -> bool {
        self.answer == Some(true) && self.allowed_at.is_some_and(|at| recent(at, now))
    }

    pub(super) fn press(&mut self, message: u32, now: u64, qualified: bool) {
        self.pressed = None;
        if qualified && self.answer.is_none() {
            self.pressed = self.event(message, now);
        }
    }

    pub(super) fn release(&mut self, message: u32, now: u64, qualified: bool) {
        let pressed = self.pressed.take();
        if !qualified || self.answer.is_some() {
            return;
        }
        if let (Some(pressed), Some(released)) = (pressed, self.event(message, now))
            && released >= pressed
            && pressed
                .checked_add(CLICK_MS)
                .is_some_and(|until| now < until)
        {
            self.answer = Some(true);
            self.allowed_at = Some(released);
        }
    }

    fn event(&self, message: u32, now: u64) -> Option<u64> {
        // Reconstruct the most recent matching low 32 bits. A future stamp
        // maps to the previous wrap, which fails the short queue-age bound.
        let base = now & !u64::from(u32::MAX);
        let reconstructed = base | u64::from(message);
        let at = if reconstructed > now {
            reconstructed.checked_sub(1_u64 << 32)?
        } else {
            reconstructed
        };
        let shown = self.shown?.checked_add(TICK_MARGIN_MS)?;
        (at > shown && recent(at, now)).then_some(at)
    }
}

fn recent(at: u64, now: u64) -> bool {
    now.checked_sub(at)
        .and_then(|age| age.checked_add(TICK_MARGIN_MS))
        .is_some_and(|age| age < QUEUE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_details_are_complete_printable_and_bounded() {
        assert!(valid_details("Verified node: nOriginal\nControl: none."));
        for details in ["", "line\0suffix", "line\rsuffix", "é", "\t"] {
            assert!(!valid_details(details));
        }
        assert!(!valid_details(&"a".repeat(79)));
        assert!(!valid_details(&"a\n".repeat(15)));
        assert!(!valid_details(&format!(
            "{}{}",
            "a".repeat(78) + "\n",
            "b".repeat(1024)
        )));
        assert!(valid_details(&("a".repeat(70) + "\n").repeat(14)));
    }

    #[test]
    fn opening_and_total_bounds_are_original_exclusive_and_sleep_inclusive() {
        let mut budget = Budget::new(7);
        assert!(budget.check(7 + OPEN_NS - 1, true));
        assert!(budget.check(7 + TOTAL_NS - 1, false));
        assert!(!budget.check(7 + TOTAL_NS, false));
        assert!(!budget.check(8, false));
        let mut opening = Budget::new(7);
        assert!(!opening.check(7 + OPEN_NS, true));
        assert!(!opening.check(7 + OPEN_NS, false));
    }

    #[test]
    fn clock_regression_or_overflow_never_reopens_a_budget() {
        let mut budget = Budget::new(7);
        assert!(budget.check(9, true));
        assert!(!budget.check(8, true));
        assert!(!budget.check(10, false));
        assert!(!Budget::new(u64::MAX - 1).check(u64::MAX, true));
    }

    fn shown() -> Choice {
        let mut choice = Choice::default();
        choice.show(1_000);
        choice
    }

    #[test]
    fn only_a_complete_post_show_qualified_mouse_click_allows_once() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.release(1_080, 1_081, true);
        assert_eq!(choice.answer(), Some(true));
        choice.press(1_090, 1_091, true);
        choice.release(1_100, 1_101, true);
        assert_eq!(choice.answer(), Some(true));
    }

    #[test]
    fn injected_or_unidentified_edges_cannot_complete_allow() {
        for (down, up) in [(false, true), (true, false), (false, false)] {
            let mut choice = shown();
            choice.press(1_020, 1_021, down);
            choice.release(1_030, 1_031, up);
            assert_eq!(choice.answer(), None);
        }
        let mut choice = shown();
        choice.release(1_030, 1_031, true);
        assert_eq!(choice.answer(), None);
    }

    #[test]
    fn old_future_or_backlogged_input_never_acquires_a_new_lifetime() {
        for (message, now) in [(999, 1_020), (1_016, 1_020), (1_100, 1_090), (1_020, 1_270)] {
            let mut choice = shown();
            choice.press(message, now, true);
            choice.release(1_300, 1_301, true);
            assert_eq!(choice.answer(), None);
        }
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.release(1_030, 1_280, true);
        assert_eq!(choice.answer(), None);
    }

    #[test]
    fn focus_capture_or_synthetic_interruption_discards_the_original_press() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.invalidate_gesture();
        choice.release(1_030, 1_031, true);
        assert_eq!(choice.answer(), None);
        choice.press(1_040, 1_041, true);
        choice.press(1_045, 1_046, false);
        choice.release(1_050, 1_051, true);
        assert_eq!(choice.answer(), None);
    }

    #[test]
    fn geometry_or_focus_round_trip_needs_a_new_click_and_cannot_restore_yes() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.context_changed();
        choice.context_changed();
        choice.release(1_030, 1_031, true);
        assert_eq!(choice.answer(), None);
        choice.press(1_040, 1_041, true);
        choice.release(1_050, 1_051, true);
        assert_eq!(choice.answer(), Some(true));
        choice.context_changed();
        choice.context_changed();
        choice.press(1_060, 1_061, true);
        choice.release(1_070, 1_071, true);
        assert_eq!(choice.answer(), Some(false));
        assert!(!choice.current_allow(1_072));
    }

    #[test]
    fn click_bound_and_order_are_exclusive_without_a_release_retry() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.release(3_020, 3_020, true);
        assert_eq!(choice.answer(), None);
        choice.release(1_030, 1_031, true);
        assert_eq!(choice.answer(), None);
        choice.press(1_040, 1_041, true);
        choice.release(1_030, 1_042, true);
        assert_eq!(choice.answer(), None);
    }

    #[test]
    fn uptime_wrap_does_not_reject_a_current_click_or_accept_an_old_one() {
        let wrap = 1_u64 << 32;
        let mut choice = Choice::default();
        choice.show(wrap - 50);
        choice.press(u32::MAX - 20, wrap - 19, true);
        choice.release(5, wrap + 6, true);
        assert_eq!(choice.answer(), Some(true));
        let mut stale = Choice::default();
        stale.show(wrap + 100);
        stale.press(u32::MAX - 20, wrap + 120, true);
        stale.release(125, wrap + 126, true);
        assert_eq!(stale.answer(), None);
    }

    #[test]
    fn nested_deny_beats_delivered_yes_and_a_replacement_has_no_old_press() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.release(1_030, 1_031, true);
        assert_eq!(choice.answer(), Some(true));
        choice.deny();
        choice.press(1_040, 1_041, true);
        choice.release(1_050, 1_051, true);
        assert_eq!(choice.answer(), Some(false));
        assert!(!choice.current_allow(1_052));

        let mut replacement = shown();
        replacement.release(1_060, 1_061, true);
        assert_eq!(replacement.answer(), None);
        replacement.press(1_070, 1_071, true);
        replacement.release(1_080, 1_081, true);
        assert_eq!(replacement.answer(), Some(true));
    }

    #[test]
    fn delayed_native_destruction_cannot_renew_a_delivered_allow() {
        let mut choice = shown();
        choice.press(1_020, 1_021, true);
        choice.release(1_030, 1_031, true);
        assert!(choice.current_allow(1_031));
        assert!(!choice.current_allow(1_029));
        assert!(choice.current_allow(1_263));
        assert!(!choice.current_allow(1_264));
        choice.press(1_265, 1_266, true);
        choice.release(1_270, 1_271, true);
        assert_eq!(choice.answer(), Some(true));
        assert!(!choice.current_allow(1_272));
        choice.deny();
        assert!(!choice.current_allow(1_031));
    }
}
