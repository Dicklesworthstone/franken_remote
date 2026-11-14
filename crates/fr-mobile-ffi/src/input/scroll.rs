//! Bounded native wheel fractions and their original exclusive deadline.
#![forbid(unsafe_code)]

/// The protocol's signed 16.16 line.
pub const LINE: i32 = 1 << 16;
/// Whole lines sent in one scroll event, per axis (frd bounds a scroll action
/// to 32 wheel steps in total).
pub const MAX_LINES_PER_EVENT: i32 = 16;

/// Fractional line accumulation for touch scrolling. Positive `y` scrolls
/// DOWN (content moves up), positive `x` scrolls RIGHT, as in the protocol.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LineScroller {
    x: f64,
    y: f64,
    /// Oldest retained fraction. Whole captured actions never wait here.
    until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRefusal {
    Invalid,
    Expired,
    OutOfRange,
}

impl LineScroller {
    /// Add one already age-bounded native capture. At most a sub-line fraction
    /// remains per axis; large whole scrolls refuse instead of queuing later
    /// actions. A combined whole line keeps the oldest fraction's deadline.
    pub fn add_captured(
        &mut self,
        dx: f64,
        dy: f64,
        now: u64,
        until: u64,
    ) -> Result<Option<(i32, i32, u64)>, ScrollRefusal> {
        if until <= now {
            self.reset();
            return Err(ScrollRefusal::Expired);
        }
        if !dx.is_finite() || !dy.is_finite() {
            self.reset();
            return Err(ScrollRefusal::Invalid);
        }
        // An untimed legacy fraction cannot enter an age-bounded capture.
        if self.until.is_none_or(|deadline| deadline <= now) {
            self.reset();
        }
        let whole = |value: f64| {
            if (value - value.round()).abs() < 1e-9 {
                value.round()
            } else {
                value.trunc()
            }
        };
        let bound = f64::from(MAX_LINES_PER_EVENT);
        if whole(self.x + dx).abs() > bound || whole(self.y + dy).abs() > bound {
            self.reset();
            return Err(ScrollRefusal::OutOfRange);
        }
        let oldest_until = self.until.map_or(until, |old| old.min(until));
        let lines = self.add(dx, dy);
        self.until = (self.remainder() != (0.0, 0.0)).then_some(oldest_until);
        Ok(lines.map(|(x, y)| (x, y, oldest_until)))
    }

    /// Add `dx`, `dy` lines; returns whole lines in 16.16 when at least one
    /// line accumulated on some axis. Non-finite input is ignored. Each axis
    /// sends at most [`MAX_LINES_PER_EVENT`] lines; the rest stays queued
    /// in the remainder for the next event, never sent fractionally. The
    /// remainder is bounded at four events' worth per axis: scrolling past
    /// that before the host takes it is dropped (a stuck or runaway
    /// gesture), not queued without limit.
    pub fn add(&mut self, dx: f64, dy: f64) -> Option<(i32, i32)> {
        if dx.is_finite() {
            self.x += dx;
        }
        if dy.is_finite() {
            self.y += dy;
        }
        // Bound a runaway remainder (a stuck gesture) to a few events' worth.
        let cap = f64::from(MAX_LINES_PER_EVENT * 4);
        self.x = self.x.clamp(-cap, cap);
        self.y = self.y.clamp(-cap, cap);
        let whole = |acc: &mut f64| {
            // Gesture deltas are binary fractions of decimal steps: a sum
            // within rounding error of a whole line counts as that line.
            let snapped = if (*acc - acc.round()).abs() < 1e-9 {
                acc.round()
            } else {
                *acc
            };
            let lines = snapped.trunc().clamp(
                -f64::from(MAX_LINES_PER_EVENT),
                f64::from(MAX_LINES_PER_EVENT),
            );
            *acc -= lines;
            // Bounded by the clamp above.
            #[allow(clippy::cast_possible_truncation)]
            let lines = lines as i32;
            lines * LINE
        };
        let (x, y) = (whole(&mut self.x), whole(&mut self.y));
        (x != 0 || y != 0).then_some((x, y))
    }
    /// Forget the remainder (a new gesture, or control ended).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub const fn remainder(&self) -> (f64, f64) {
        (self.x, self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_fractions_keep_the_original_deadline() {
        let mut scroll = LineScroller::default();
        assert_eq!(scroll.add_captured(0.0, 0.6, 1_000, 1_100), Ok(None));
        assert_eq!(
            scroll.add_captured(0.0, 0.6, 1_040, 1_140),
            Ok(Some((0, LINE, 1_100)))
        );
        // The remaining 0.2 line is still part of the original capture.
        assert_eq!(
            scroll.add_captured(0.0, 0.8, 1_099, 1_199),
            Ok(Some((0, LINE, 1_100)))
        );
        assert!(scroll.remainder().0.abs() < 1e-9 && scroll.remainder().1.abs() < 1e-9);
        assert_eq!(scroll.add_captured(0.0, 0.6, 1_100, 1_200), Ok(None));
        assert_eq!(
            scroll.add_captured(0.0, 0.4, 1_120, 1_220),
            Ok(Some((0, LINE, 1_200)))
        );
    }

    #[test]
    fn exact_expiry_discards_old_fraction_instead_of_renewing_it() {
        for now in [1_100, 1_101, 80_000] {
            let mut scroll = LineScroller::default();
            assert_eq!(scroll.add_captured(0.0, 0.6, 1_000, 1_100), Ok(None));
            assert_eq!(scroll.add_captured(0.0, 0.6, now, now + 100), Ok(None));
            assert_eq!(
                scroll.add_captured(0.0, 0.4, now + 1, now + 101),
                Ok(Some((0, LINE, now + 100)))
            );
        }
    }

    #[test]
    fn intervening_action_discards_fraction_and_its_bound() {
        let mut scroll = LineScroller::default();
        assert_eq!(scroll.add_captured(0.6, 0.0, 1_000, 1_100), Ok(None));
        scroll.reset();
        assert_eq!(scroll.add_captured(0.6, 0.0, 1_010, 1_110), Ok(None));
        assert_eq!(
            scroll.add_captured(0.4, 0.0, 1_020, 1_120),
            Ok(Some((LINE, 0, 1_110)))
        );
    }

    #[test]
    fn failed_capture_cannot_mutate_or_retain_fractional_input() {
        for (dx, dy, until, refusal) in [
            (0.0, 0.5, 1_001, ScrollRefusal::Expired),
            (f64::NAN, 0.5, 1_100, ScrollRefusal::Invalid),
            (0.0, f64::INFINITY, 1_100, ScrollRefusal::Invalid),
            (17.0, 0.5, 1_100, ScrollRefusal::OutOfRange),
            (0.0, -18.0, 1_100, ScrollRefusal::OutOfRange),
        ] {
            let mut scroll = LineScroller::default();
            assert_eq!(scroll.add_captured(0.0, 0.6, 1_000, 1_100), Ok(None));
            assert_eq!(scroll.add_captured(dx, dy, 1_001, until), Err(refusal));
            assert_eq!(scroll.remainder(), (0.0, 0.0));
            assert_eq!(scroll.add_captured(0.0, 0.6, 1_002, 1_102), Ok(None));
        }
    }

    #[test]
    fn newer_capture_can_only_shorten_the_combined_deadline() {
        let mut scroll = LineScroller::default();
        assert_eq!(scroll.add_captured(0.0, -0.4, 1_000, 1_100), Ok(None));
        assert_eq!(
            scroll.add_captured(0.0, -0.6, 1_010, 1_020),
            Ok(Some((0, -LINE, 1_020)))
        );
    }

    #[test]
    fn axes_and_native_step_bounds_stay_independent() {
        let mut scroll = LineScroller::default();
        assert_eq!(
            scroll.add_captured(16.0, -16.0, 1_000, 1_100),
            Ok(Some((16 * LINE, -16 * LINE, 1_100)))
        );
        assert_eq!(scroll.remainder(), (0.0, 0.0));
        // No whole overflow remains for an unrelated later event.
        assert_eq!(
            scroll.add_captured(40.0, 0.0, 1_010, 1_110),
            Err(ScrollRefusal::OutOfRange)
        );
        assert_eq!(scroll.add_captured(0.0, 0.0, 1_011, 1_111), Ok(None));
    }
}
