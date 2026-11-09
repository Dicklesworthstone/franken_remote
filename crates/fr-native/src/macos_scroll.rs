//! Native scroll distances and bounded fractional carry. No OS calls, display
//! scale, guessed pixel/line conversion, retained action, or timer lives here.
#![forbid(unsafe_code)]

use fr_core::{
    input::ScrollUnit,
    input_submission::{
        Capabilities, Capability,
        scroll::{LINE, LineScroll},
    },
};
use std::time::Duration;

/// A fixed correlation, sampled once when the native producer attaches. The
/// client clock is sampled BEFORE the native clock, making mapped event times
/// conservative. Repeated reads cannot rejuvenate old native events or make
/// timestamps regress when another event was submitted between scrolls.
pub(crate) struct CaptureClock {
    native: Duration,
    client: u64,
}

impl CaptureClock {
    pub(crate) const fn new(native: Duration, client: u64) -> Self {
        Self { native, client }
    }

    pub(crate) fn map(&self, captured: Duration) -> Option<u64> {
        if let Some(delta) = captured.checked_sub(self.native) {
            self.client
                .checked_add(u64::try_from(delta.as_micros()).ok()?)
        } else {
            // Round age up so quantization never makes an old event younger.
            let age = self
                .native
                .checked_sub(captured)?
                .as_nanos()
                .div_ceil(1_000);
            self.client.checked_sub(u64::try_from(age).ok()?)
        }
    }
}

/// Content-free local refusal. Unsupported input leaves other control usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Unsupported,
    Invalid,
    OutOfRange,
    Expired,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Delta {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) unit: ScrollUnit,
    /// Oldest contribution, in the Source's original client clock domain.
    pub(crate) sampled: u64,
}

impl core::fmt::Debug for Delta {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ScrollDelta([redacted])")
    }
}

/// At most one sub-unit remainder per axis: less than one wire quantum for
/// pixels or one native line. An intervening action, changed unit, expired
/// remainder, refusal or producer drop clears it. No whole scroll is retained.
pub(crate) struct Scroll {
    remainder: (f64, f64),
    unit: Option<ScrollUnit>,
    oldest: Option<u64>,
    last: Option<u64>,
    max_age_us: u64,
}

impl Scroll {
    pub(crate) const fn new(max_age_us: u64) -> Self {
        Self {
            remainder: (0.0, 0.0),
            unit: None,
            oldest: None,
            last: None,
            max_age_us,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.remainder = (0.0, 0.0);
        self.unit = None;
        self.oldest = None;
        self.last = None;
    }

    pub(crate) fn push(
        &mut self,
        delta: (f64, f64),
        precise: bool,
        capabilities: Capabilities,
        sampled: u64,
        now: u64,
    ) -> Result<Option<Delta>, Refusal> {
        let result = self.convert(delta, precise, capabilities, sampled, now);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn convert(
        &mut self,
        (dx, dy): (f64, f64),
        precise: bool,
        capabilities: Capabilities,
        sampled: u64,
        now: u64,
    ) -> Result<Option<Delta>, Refusal> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(Refusal::Invalid);
        }
        if now
            .checked_sub(sampled)
            .is_none_or(|age| age >= self.max_age_us)
            || self.last.is_some_and(|last| sampled < last)
        {
            return Err(Refusal::Expired);
        }
        let (unit, required, scale) = if precise {
            (ScrollUnit::Pixels, Capability::PixelScroll, f64::from(LINE))
        } else {
            (ScrollUnit::Lines, Capability::LineScroll, 1.0)
        };
        if !capabilities.contains(required) {
            return Err(Refusal::Unsupported);
        }
        if self.unit != Some(unit)
            || self
                .oldest
                .is_some_and(|oldest| now - oldest >= self.max_age_us)
        {
            self.reset();
        }
        self.unit = Some(unit);
        self.last = Some(sampled);
        // AppKit's positive delta scrolls up/left; the protocol is down/right.
        let candidate = (self.remainder.0 - dx * scale, self.remainder.1 - dy * scale);
        let whole = (candidate.0.trunc(), candidate.1.trunc());
        let wire_scale = if precise { 1.0 } else { f64::from(LINE) };
        let wire = (whole.0 * wire_scale, whole.1 * wire_scale);
        let bound = f64::from(i32::MAX);
        if !(-bound..=bound).contains(&wire.0) || !(-bound..=bound).contains(&wire.1) {
            return Err(Refusal::OutOfRange);
        }
        // Finite and checked in the symmetric i32 range; fractions were
        // removed above, and the unnegatable i32::MIN is deliberately excluded.
        #[allow(clippy::cast_possible_truncation)]
        let (x, y) = (wire.0 as i32, wire.1 as i32);
        if !precise && LineScroll::new(x, y).is_none() {
            return Err(Refusal::OutOfRange);
        }
        let oldest = self.oldest.unwrap_or(sampled);
        self.remainder = (candidate.0 - whole.0, candidate.1 - whole.1);
        self.oldest = (self.remainder != (0.0, 0.0)).then_some(oldest);
        if x == 0 && y == 0 {
            return Ok(None);
        }
        Ok(Some(Delta {
            x,
            y,
            unit,
            sampled: oldest,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGE: u64 = 100_000;
    fn caps() -> Capabilities {
        Capabilities::default()
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll)
    }

    #[test]
    fn native_clock_correlation_is_stable_and_conservative() {
        let clock = CaptureClock::new(Duration::from_secs(20), 1_000_000);
        assert_eq!(clock.map(Duration::from_secs(20)), Some(1_000_000));
        assert_eq!(
            clock.map(Duration::from_nanos(19_999_999_999)),
            Some(999_999)
        );
        assert_eq!(
            clock.map(Duration::from_nanos(20_000_001_999)),
            Some(1_000_001)
        );
        assert_eq!(clock.map(Duration::ZERO), None);
    }

    #[test]
    fn precise_axes_preserve_native_distance_and_one_wire_quantum() {
        let mut scroll = Scroll::new(AGE);
        assert_eq!(
            scroll.push((1.5, -2.25), true, caps(), 10, 20),
            Ok(Some(Delta {
                x: -3 * LINE / 2,
                y: 9 * LINE / 4,
                unit: ScrollUnit::Pixels,
                sampled: 10,
            }))
        );
        assert_eq!(
            scroll
                .push((1.0 / f64::from(LINE), 0.0), true, caps(), 21, 22)
                .unwrap()
                .unwrap()
                .x,
            -1
        );
    }

    #[test]
    fn fractional_carry_keeps_oldest_time_and_does_not_retain_whole_events() {
        let mut scroll = Scroll::new(AGE);
        let half = 0.5 / f64::from(LINE);
        assert_eq!(scroll.push((half, 0.0), true, caps(), 10, 20), Ok(None));
        assert_eq!(
            scroll.push((half, 0.0), true, caps(), 30, 40),
            Ok(Some(Delta {
                x: -1,
                y: 0,
                unit: ScrollUnit::Pixels,
                sampled: 10,
            }))
        );
        assert_eq!(scroll.push((0.0, 0.0), true, caps(), 50, 60), Ok(None));
    }

    #[test]
    fn line_carry_is_native_lines_and_obeys_combined_host_bound() {
        let mut scroll = Scroll::new(AGE);
        assert_eq!(scroll.push((0.25, -0.5), false, caps(), 10, 20), Ok(None));
        assert_eq!(
            scroll.push((0.75, -0.5), false, caps(), 30, 40),
            Ok(Some(Delta {
                x: -LINE,
                y: LINE,
                unit: ScrollUnit::Lines,
                sampled: 10,
            }))
        );
        assert!(
            scroll
                .push((16.0, -16.0), false, caps(), 50, 60)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            scroll.push((17.0, 16.0), false, caps(), 70, 80),
            Err(Refusal::OutOfRange)
        );
    }

    #[test]
    fn absent_mode_never_converts_scroll_units_or_leaks_carry() {
        let mut scroll = Scroll::new(AGE);
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), 10, 20), Ok(None));
        let lines = Capabilities::default().with(Capability::LineScroll);
        assert_eq!(
            scroll.push((12.0, 0.0), true, lines, 30, 40),
            Err(Refusal::Unsupported)
        );
        assert_eq!(scroll.push((0.5, 0.0), false, lines, 50, 60), Ok(None));
        assert_eq!(
            scroll.push(
                (1.0, 0.0),
                false,
                Capabilities::default().with(Capability::PixelScroll),
                70,
                80
            ),
            Err(Refusal::Unsupported)
        );
    }

    #[test]
    fn stale_regressed_and_invalid_events_cannot_refresh_a_remainder() {
        let mut scroll = Scroll::new(AGE);
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), 0, 1), Ok(None));
        // A fresh event after the old fraction expired cannot replay it.
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), AGE, AGE), Ok(None));
        assert_eq!(
            scroll.push((0.5, 0.0), false, caps(), AGE - 1, AGE),
            Err(Refusal::Expired)
        );
        assert_eq!(
            scroll.push((1.0, 0.0), true, caps(), 0, AGE),
            Err(Refusal::Expired)
        );
        assert_eq!(
            scroll.push((f64::NAN, 0.0), true, caps(), AGE, AGE),
            Err(Refusal::Invalid)
        );
        assert_eq!(
            scroll.push((f64::INFINITY, 0.0), false, caps(), AGE, AGE),
            Err(Refusal::Invalid)
        );
        assert_eq!(
            scroll.push((32768.0, 0.0), true, caps(), AGE, AGE),
            Err(Refusal::OutOfRange)
        );
    }

    #[test]
    fn action_barriers_and_mode_changes_reset_fractional_carry() {
        let mut scroll = Scroll::new(AGE);
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), 10, 20), Ok(None));
        scroll.reset();
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), 30, 40), Ok(None));
        let delta = scroll
            .push((1.0, 0.0), true, caps(), 50, 60)
            .unwrap()
            .unwrap();
        assert_eq!((delta.x, delta.sampled), (-LINE, 50));
        assert_eq!(scroll.push((0.5, 0.0), false, caps(), 70, 80), Ok(None));
    }
}
