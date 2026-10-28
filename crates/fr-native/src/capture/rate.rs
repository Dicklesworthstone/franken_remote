//! Requested software rate is not evidence of an applied encoder change.
//! The capture owner applies only on useful native work, after a real bootstrap.
//! Both clocks must witness two seconds after the last validated transition.
use std::time::{Duration, Instant};

const DWELL: Duration = Duration::from_secs(2);

pub(super) struct Rate {
    requested: u32,
    applied: u32,
    last: Option<(u64, Instant)>,
}
impl Rate {
    pub(super) const fn new(initial: u32) -> Self {
        Self {
            requested: initial,
            applied: initial,
            last: None,
        }
    }
    /// The encoder's software/range check must precede this installation.
    pub(super) const fn request(&mut self, bitrate: u32) {
        self.requested = bitrate;
    }
    pub(super) fn due(&self, host: u64, worker: Instant, bootstrapped: bool) -> Option<u32> {
        if !bootstrapped || self.requested == self.applied {
            return None;
        }
        if let Some((last_host, last_worker)) = self.last
            && (host
                .checked_sub(last_host)
                .map(Duration::from_micros)
                .is_none_or(|d| d < DWELL)
                || worker
                    .checked_duration_since(last_worker)
                    .is_none_or(|d| d < DWELL))
        {
            return None;
        }
        Some(self.requested)
    }
    /// Only after the new IDR and original display/source have both validated.
    pub(super) const fn applied(&mut self, bitrate: u32, host: u64, worker: Instant) {
        self.applied = bitrate;
        self.last = Some((host, worker));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_precedes_first_change_and_only_success_charges_dwell() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.request(4_000_000);
        assert_eq!(rate.due(10, start, false), None);
        for _ in 0..10 {
            assert_eq!(rate.due(10, start, true), Some(4_000_000));
        }
        rate.applied(4_000_000, 10, start);
        assert_eq!(rate.due(10, start, true), None);
        rate.request(2_000_000);
        assert_eq!(rate.due(10, start, true), None);
    }
    #[test]
    fn both_original_clocks_must_reach_the_exact_dwell_boundary() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.applied(4_000_000, 10, start);
        rate.request(2_000_000);
        let late = start + Duration::from_micros(2_000_000);
        assert_eq!(rate.due(2_000_009, late, true), None);
        assert_eq!(
            rate.due(
                2_000_010,
                late.checked_sub(Duration::from_micros(1)).unwrap(),
                true
            ),
            None
        );
        assert_eq!(rate.due(2_000_010, late, true), Some(2_000_000));
        assert_eq!(rate.due(9, late, true), None);
        assert_eq!(
            rate.due(
                2_000_010,
                start.checked_sub(Duration::from_micros(1)).unwrap(),
                true
            ),
            None
        );
    }
    #[test]
    fn retargeting_does_not_replenish_or_extend_the_original_dwell() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.applied(4_000_000, 0, start);
        for target in [2_000_000, 1_000_000, 8_000_000, 4_000_000, 2_000_000] {
            rate.request(target);
            assert_eq!(rate.due(1_999_999, start + DWELL, true), None);
        }
        assert_eq!(rate.due(2_000_000, start + DWELL, true), Some(2_000_000));
        rate.request(4_000_000);
        assert_eq!(rate.due(2_000_000, start + DWELL, true), None);
    }
}
