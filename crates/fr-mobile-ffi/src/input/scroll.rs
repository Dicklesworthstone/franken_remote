//! Bounded native wheel fractions and their original exclusive deadline.
#![forbid(unsafe_code)]

use fr_core::{
    input::ScrollUnit,
    input_submission::{Capabilities, Capability},
};

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
    Unsupported,
}

/// One original scroll contribution. Distances are private input, so this
/// type deliberately has no diagnostic formatter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScrollDelta {
    pub x: i32,
    pub y: i32,
    pub unit: ScrollUnit,
    pub sampled: u64,
    pub until: u64,
}

/// The granted producer retains at most one sub-unit fraction per axis.
/// Lines use whole native steps; precise scroll uses one 16.16 wire quantum.
/// Neither mode converts into the other or retains a whole event for retry.
#[derive(Default)]
pub struct ScrollAccumulator {
    remainder: (f64, f64),
    unit: Option<ScrollUnit>,
    oldest: Option<(u64, u64)>,
    last_sample: Option<u64>,
}

impl ScrollAccumulator {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Add a newly captured delta under its original exclusive deadline.
    /// The session owns source/view identity and resets this accumulator when
    /// that identity changes, before submitting through the same context.
    pub fn add_captured(
        &mut self,
        unit: ScrollUnit,
        delta: (f64, f64),
        captured: (u64, u64),
        now: u64,
        capabilities: Capabilities,
    ) -> Result<Option<ScrollDelta>, ScrollRefusal> {
        let result = self.convert(unit, delta, captured, now, capabilities);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn convert(
        &mut self,
        unit: ScrollUnit,
        (dx, dy): (f64, f64),
        (sampled, until): (u64, u64),
        now: u64,
        capabilities: Capabilities,
    ) -> Result<Option<ScrollDelta>, ScrollRefusal> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(ScrollRefusal::Invalid);
        }
        if sampled > now || now >= until || self.last_sample.is_some_and(|last| sampled < last) {
            return Err(ScrollRefusal::Expired);
        }
        let required = match unit {
            ScrollUnit::Lines => Capability::LineScroll,
            ScrollUnit::Pixels => Capability::PixelScroll,
        };
        if !capabilities.contains(required) {
            return Err(ScrollRefusal::Unsupported);
        }
        if self.unit != Some(unit) || self.oldest.is_some_and(|(_, deadline)| now >= deadline) {
            self.reset();
        }
        self.unit = Some(unit);
        self.last_sample = Some(sampled);
        let scale = match unit {
            ScrollUnit::Lines => 1.0,
            ScrollUnit::Pixels => f64::from(LINE),
        };
        let candidate = (self.remainder.0 + dx * scale, self.remainder.1 + dy * scale);
        let whole = |value: f64| {
            if unit == ScrollUnit::Lines && (value - value.round()).abs() < 1e-9 {
                value.round()
            } else {
                value.trunc()
            }
        };
        let (x, y) = (whole(candidate.0), whole(candidate.1));
        let bound = match unit {
            ScrollUnit::Lines => f64::from(MAX_LINES_PER_EVENT),
            // Exclude i32::MIN: native sign conversion must be representable.
            ScrollUnit::Pixels => f64::from(i32::MAX),
        };
        let admitted = if unit == ScrollUnit::Pixels {
            candidate
        } else {
            (x, y)
        };
        if !(-bound..=bound).contains(&admitted.0) || !(-bound..=bound).contains(&admitted.1) {
            return Err(ScrollRefusal::OutOfRange);
        }
        let (oldest_sample, oldest_until) =
            self.oldest.map_or((sampled, until), |(old, deadline)| {
                (old, deadline.min(until))
            });
        self.remainder = (candidate.0 - x, candidate.1 - y);
        self.oldest = (self.remainder != (0.0, 0.0)).then_some((oldest_sample, oldest_until));
        if x == 0.0 && y == 0.0 {
            return Ok(None);
        }
        let wire_scale = if unit == ScrollUnit::Lines { LINE } else { 1 };
        // Finite integral values checked against the symmetric range above.
        #[allow(clippy::cast_possible_truncation)]
        let (x, y) = (x as i32 * wire_scale, y as i32 * wire_scale);
        Ok(Some(ScrollDelta {
            x,
            y,
            unit,
            sampled: oldest_sample,
            until: oldest_until,
        }))
    }
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

    fn capabilities() -> Capabilities {
        Capabilities::default()
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll)
    }

    fn values(delta: ScrollDelta) -> (i32, i32, ScrollUnit, u64, u64) {
        (delta.x, delta.y, delta.unit, delta.sampled, delta.until)
    }

    #[test]
    fn precise_scroll_preserves_signed_logical_distance_without_display_scaling() {
        let mut scroll = ScrollAccumulator::default();
        let delta = scroll
            .add_captured(
                ScrollUnit::Pixels,
                (1.5, -2.25),
                (10, 100),
                20,
                capabilities(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            values(delta),
            (3 * LINE / 2, -9 * LINE / 4, ScrollUnit::Pixels, 10, 100)
        );
        assert_eq!(scroll.remainder, (0.0, 0.0));
        assert!(scroll.oldest.is_none());
    }

    #[test]
    fn precise_subquantum_carry_keeps_oldest_sample_and_shortest_deadline() {
        let mut scroll = ScrollAccumulator::default();
        let half = 0.5 / f64::from(LINE);
        assert!(
            scroll
                .add_captured(
                    ScrollUnit::Pixels,
                    (half, -half),
                    (10, 100),
                    20,
                    capabilities()
                )
                .unwrap()
                .is_none()
        );
        let delta = scroll
            .add_captured(
                ScrollUnit::Pixels,
                (half, -half),
                (30, 80),
                40,
                capabilities(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(values(delta), (1, -1, ScrollUnit::Pixels, 10, 80));
        assert_eq!(scroll.remainder, (0.0, 0.0));
    }

    #[test]
    fn expired_fraction_never_acquires_the_next_events_lifetime() {
        for unit in [ScrollUnit::Lines, ScrollUnit::Pixels] {
            let half = if unit == ScrollUnit::Lines {
                0.5
            } else {
                0.5 / f64::from(LINE)
            };
            let mut scroll = ScrollAccumulator::default();
            assert!(
                scroll
                    .add_captured(unit, (0.0, half), (10, 50), 20, capabilities())
                    .unwrap()
                    .is_none()
            );
            assert!(
                scroll
                    .add_captured(unit, (0.0, half), (50, 90), 50, capabilities())
                    .unwrap()
                    .is_none()
            );
            let delta = scroll
                .add_captured(unit, (0.0, half), (60, 100), 70, capabilities())
                .unwrap()
                .unwrap();
            assert_eq!(
                values(delta),
                (
                    0,
                    if unit == ScrollUnit::Lines { LINE } else { 1 },
                    unit,
                    50,
                    90
                )
            );
        }
    }

    #[test]
    fn unit_changes_and_intervening_actions_discard_retained_fractions() {
        let mut scroll = ScrollAccumulator::default();
        assert!(
            scroll
                .add_captured(
                    ScrollUnit::Lines,
                    (0.0, 0.75),
                    (10, 100),
                    20,
                    capabilities()
                )
                .unwrap()
                .is_none()
        );
        let delta = scroll
            .add_captured(
                ScrollUnit::Pixels,
                (0.0, 0.5),
                (30, 110),
                40,
                capabilities(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(values(delta), (0, LINE / 2, ScrollUnit::Pixels, 30, 110));
        assert!(
            scroll
                .add_captured(
                    ScrollUnit::Lines,
                    (0.0, 0.75),
                    (50, 150),
                    60,
                    capabilities()
                )
                .unwrap()
                .is_none()
        );
        scroll.reset();
        assert!(
            scroll
                .add_captured(ScrollUnit::Lines, (0.0, 0.5), (70, 170), 80, capabilities())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_line_only_host_cannot_authorize_precise_scroll_or_retain_its_remainder() {
        for (unit, missing) in [
            (ScrollUnit::Pixels, Capability::LineScroll),
            (ScrollUnit::Lines, Capability::PixelScroll),
        ] {
            let mut scroll = ScrollAccumulator::default();
            let result = scroll.add_captured(
                unit,
                (0.0, 0.5),
                (10, 100),
                20,
                Capabilities::default().with(missing),
            );
            assert!(matches!(result, Err(ScrollRefusal::Unsupported)));
            assert_eq!(scroll.remainder, (0.0, 0.0));
            assert!(scroll.unit.is_none());
            assert!(scroll.oldest.is_none());
        }
    }

    #[test]
    fn precise_overflow_nonfinite_and_expired_events_clear_state_without_carrying_whole_work() {
        for (delta, captured, now, error) in [
            ((f64::NAN, 0.0), (20, 100), 30, ScrollRefusal::Invalid),
            ((0.0, f64::INFINITY), (20, 100), 30, ScrollRefusal::Invalid),
            ((f64::MAX, 0.0), (20, 100), 30, ScrollRefusal::OutOfRange),
            ((-32768.0, 0.0), (20, 100), 30, ScrollRefusal::OutOfRange),
            ((0.0, 32768.0), (20, 100), 30, ScrollRefusal::OutOfRange),
            ((0.0, 0.5), (20, 30), 30, ScrollRefusal::Expired),
            ((0.0, 0.5), (31, 100), 30, ScrollRefusal::Expired),
            ((0.0, 0.5), (9, 100), 30, ScrollRefusal::Expired),
        ] {
            let mut scroll = ScrollAccumulator::default();
            assert!(
                scroll
                    .add_captured(
                        ScrollUnit::Pixels,
                        (0.5 / f64::from(LINE), 0.0),
                        (10, 100),
                        10,
                        capabilities()
                    )
                    .unwrap()
                    .is_none()
            );
            assert!(
                matches!(scroll.add_captured(ScrollUnit::Pixels, delta, captured, now, capabilities()), Err(found) if found == error)
            );
            assert_eq!(scroll.remainder, (0.0, 0.0));
            assert!(
                scroll
                    .add_captured(
                        ScrollUnit::Pixels,
                        (0.0, 0.0),
                        (40, 140),
                        50,
                        capabilities()
                    )
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn both_units_preserve_their_own_wire_limits_without_large_event_backlog() {
        let mut scroll = ScrollAccumulator::default();
        let max = f64::from(i32::MAX) / f64::from(LINE);
        let precise = scroll
            .add_captured(
                ScrollUnit::Pixels,
                (max, -max),
                (10, 100),
                20,
                capabilities(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            values(precise),
            (i32::MAX, -i32::MAX, ScrollUnit::Pixels, 10, 100)
        );
        let lines = scroll
            .add_captured(
                ScrollUnit::Lines,
                (16.0, -16.0),
                (30, 110),
                40,
                capabilities(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            values(lines),
            (16 * LINE, -16 * LINE, ScrollUnit::Lines, 30, 110)
        );
        assert!(matches!(
            scroll.add_captured(
                ScrollUnit::Lines,
                (17.0, 0.0),
                (50, 150),
                60,
                capabilities()
            ),
            Err(ScrollRefusal::OutOfRange)
        ));
        assert!(
            scroll
                .add_captured(ScrollUnit::Lines, (0.0, 0.0), (70, 170), 80, capabilities())
                .unwrap()
                .is_none()
        );
    }

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
