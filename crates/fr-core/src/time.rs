//! Host-monotonic time as data (plan sections 6.3, 13.2; PROTOCOL.md §2).
//!
//! Authority decisions are functions of instants supplied by the caller, so
//! the state machines in this crate never read a clock themselves. Wire
//! timestamps are `u64` microseconds; an instant is meaningful only within
//! its clock domain (one host boot), and suspend/resume is an explicit
//! authority boundary handled by [`crate::authority`], never inferred from
//! the numbers.

use core::fmt;

/// A point on the host's monotonic authority clock, in microseconds since
/// an arbitrary per-boot origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostInstant(u64);

impl HostInstant {
    /// The clock origin (used by tests and the epoch boundary).
    pub const ORIGIN: Self = Self(0);

    /// Wraps a raw microsecond reading from the qualified platform clock.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Microseconds since the per-boot origin, for wire encoding.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// `self + d`, or `None` on overflow. Deadlines never wrap.
    #[must_use]
    pub const fn checked_add(self, d: HostDuration) -> Option<Self> {
        match self.0.checked_add(d.as_micros()) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// The duration from `earlier` to `self`, or `None` when `earlier` is
    /// actually later (callers decide whether that is a fault).
    #[must_use]
    pub const fn checked_duration_since(self, earlier: Self) -> Option<HostDuration> {
        match self.0.checked_sub(earlier.0) {
            Some(v) => Some(HostDuration::from_micros(v)),
            None => None,
        }
    }
}

impl fmt::Display for HostInstant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}us", self.0)
    }
}

/// A span of host-monotonic time in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostDuration(u64);

impl HostDuration {
    /// Zero-length span.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw microsecond count.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Whole milliseconds, saturating multiplication guarded by the caller's
    /// realistic ranges (checked: overflow yields `None`).
    #[must_use]
    pub const fn from_millis_checked(millis: u64) -> Option<Self> {
        match millis.checked_mul(1_000) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// The raw microsecond count.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }
}

impl fmt::Display for HostDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}us", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_arithmetic_is_checked() {
        let t = HostInstant::from_micros(10);
        let d = HostDuration::from_micros(5);
        assert_eq!(t.checked_add(d), Some(HostInstant::from_micros(15)));
        assert_eq!(HostInstant::from_micros(u64::MAX).checked_add(d), None);
        assert_eq!(
            HostInstant::from_micros(15).checked_duration_since(t),
            Some(HostDuration::from_micros(5))
        );
        assert_eq!(t.checked_duration_since(HostInstant::from_micros(15)), None);
    }

    #[test]
    fn millis_conversion_is_checked() {
        assert_eq!(
            HostDuration::from_millis_checked(1_500),
            Some(HostDuration::from_micros(1_500_000))
        );
        assert_eq!(HostDuration::from_millis_checked(u64::MAX), None);
    }
}
