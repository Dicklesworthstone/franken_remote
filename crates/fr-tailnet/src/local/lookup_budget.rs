//! The fixed installed-app lookup budget, including queueing and system sleep.
use crate::Error;

const LOOKUP_US: u64 = 1_000_000;

#[derive(Clone)]
#[allow(clippy::struct_field_names)] // Every field is a microsecond instant.
pub(super) struct Clock {
    pub(super) issued_us: u64,
    pub(super) wall_issued_us: u64,
    end_us: u64,
    wall_end_us: u64,
    previous_us: u64,
    previous_wall_us: u64,
}
impl Clock {
    pub(super) fn new(issued_us: u64, wall_issued_us: u64) -> Result<Self, Error> {
        Ok(Self {
            issued_us,
            wall_issued_us,
            end_us: issued_us.checked_add(LOOKUP_US).ok_or(Error::Clock)?,
            wall_end_us: wall_issued_us.checked_add(LOOKUP_US).ok_or(Error::Clock)?,
            previous_us: issued_us,
            previous_wall_us: wall_issued_us,
        })
    }
    pub(super) const fn end_us(&self) -> u64 {
        self.end_us
    }
    pub(super) fn check(&mut self, current_us: u64, wall_us: u64) -> Result<(), Error> {
        if current_us < self.previous_us || wall_us < self.previous_wall_us {
            return Err(Error::Clock);
        }
        self.previous_us = current_us;
        self.previous_wall_us = wall_us;
        if current_us >= self.end_us || wall_us >= self.wall_end_us {
            return Err(Error::Timeout);
        }
        Ok(())
    }
}

/// This clock is an additional expiry ceiling, never a source of fresh time.
/// It prevents a paused monotonic clock preserving a destination across sleep.
pub(super) fn check_wall_lifetime(
    issued_us: u64,
    current_us: u64,
    validity_us: u64,
) -> Result<(), Error> {
    if current_us < issued_us {
        return Err(Error::Clock);
    }
    let end = issued_us.checked_add(validity_us).ok_or(Error::Clock)?;
    if current_us >= end {
        return Err(Error::Expired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_keeps_the_original_one_second_deadline() {
        let mut clock = Clock::new(7, 70).unwrap();
        assert_eq!((clock.issued_us, clock.wall_issued_us), (7, 70));
        assert_eq!(clock.end_us(), 1_000_007);
        for elapsed in [0, 100_000, 400_000, 999_999] {
            clock.check(7 + elapsed, 70 + elapsed).unwrap();
        }
        assert_eq!(clock.check(1_000_007, 1_000_070), Err(Error::Timeout));
        assert_eq!(clock.check(1_000_008, 1_000_071), Err(Error::Timeout));
    }

    #[test]
    fn a_paused_monotonic_clock_never_preserves_a_lookup_across_sleep() {
        let mut clock = Clock::new(7, 70).unwrap();
        assert_eq!(clock.check(8, 1_000_070), Err(Error::Timeout));
        assert_eq!(clock.check(9, 1_000_071), Err(Error::Timeout));
    }

    #[test]
    fn clock_rollback_and_overflow_refuse_instead_of_extending_the_budget() {
        for (current, wall) in [(6, 70), (7, 69), (8, 71)] {
            let mut clock = Clock::new(7, 70).unwrap();
            clock.check(10, 80).unwrap();
            assert_eq!(clock.check(current, wall), Err(Error::Clock));
        }
        assert!(matches!(Clock::new(u64::MAX, 0), Err(Error::Clock)));
        assert!(matches!(Clock::new(0, u64::MAX), Err(Error::Clock)));
    }

    #[test]
    fn destination_wall_ceiling_cannot_renew_a_suspended_or_reversed_clock() {
        assert_eq!(check_wall_lifetime(70, 3_000_069, 3_000_000), Ok(()));
        assert_eq!(
            check_wall_lifetime(70, 3_000_070, 3_000_000),
            Err(Error::Expired)
        );
        assert_eq!(check_wall_lifetime(70, 69, 3_000_000), Err(Error::Clock));
        assert_eq!(
            check_wall_lifetime(u64::MAX, u64::MAX, 1),
            Err(Error::Clock)
        );
        assert_eq!(check_wall_lifetime(70, 70, 0), Err(Error::Expired));
    }
}
