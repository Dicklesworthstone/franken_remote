//! A requested rate is not evidence of an applied encoder change.
//! The capture owner applies only on useful native work, after a real bootstrap.
//! Both clocks must witness two seconds after the last validated transition.
use std::time::{Duration, Instant};

const DWELL: Duration = Duration::from_secs(2);

pub(crate) struct Rate {
    requested: u32,
    applied: u32,
    last: Option<(u64, Instant)>,
}
impl Rate {
    pub(crate) const fn new(initial: u32) -> Self {
        Self {
            requested: initial,
            applied: initial,
            last: None,
        }
    }
    /// The encoder's backend/range check must precede this installation.
    pub(crate) const fn request(&mut self, bitrate: u32) {
        self.requested = bitrate;
    }
    /// The original native configuration remains the ceiling, even after a
    /// lower rate has been applied. Invalid requests cannot replace a pending
    /// valid target or consume its dwell.
    #[cfg(any(test, all(windows, feature = "windows-media")))]
    pub(crate) fn request_with_ceiling(&mut self, bitrate: u32, ceiling: u32) -> Result<(), ()> {
        if !(10_000..=200_000_000).contains(&ceiling) || !(10_000..=ceiling).contains(&bitrate) {
            return Err(());
        }
        self.request(bitrate);
        Ok(())
    }
    #[cfg(any(test, all(windows, feature = "windows-media")))]
    pub(crate) const fn applied_bitrate(&self) -> u32 {
        self.applied
    }
    pub(crate) fn due(&self, host: u64, worker: Instant, bootstrapped: bool) -> Option<u32> {
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
    pub(crate) const fn applied(&mut self, bitrate: u32, host: u64, worker: Instant) {
        self.applied = bitrate;
        self.last = Some((host, worker));
    }
    /// Confirm a synchronous native transaction only after source and bitstream
    /// validation. An unchanged/P-picture result can retain the old setting,
    /// but cannot claim a changed rate. Deferral never charges a new dwell.
    #[cfg(any(
        test,
        all(target_os = "macos", feature = "macos-media"),
        all(windows, feature = "windows-media")
    ))]
    pub(crate) fn confirm(
        &mut self,
        target: u32,
        actual: u32,
        idr: bool,
        host: u64,
        worker: Instant,
    ) -> Result<(), ()> {
        if actual != self.applied {
            if actual != target || !idr {
                return Err(());
            }
            self.applied(actual, host, worker);
        }
        Ok(())
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

    #[test]
    fn an_idle_or_deferred_capture_retains_the_target_without_applying_or_charging_it() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.request(4_000_000);
        for host in 0..10 {
            assert_eq!(rate.due(host, start, true), Some(4_000_000));
            rate.confirm(4_000_000, 8_000_000, false, host, start)
                .unwrap();
            assert_eq!(rate.applied, 8_000_000);
            assert_eq!(rate.last, None);
        }
        rate.confirm(4_000_000, 4_000_000, true, 10, start).unwrap();
        assert_eq!(rate.applied, 4_000_000);
        rate.request(2_000_000);
        assert_eq!(rate.due(2_000_009, start + DWELL, true), None);
        assert_eq!(rate.due(2_000_010, start + DWELL, true), Some(2_000_000));
    }

    #[test]
    fn a_rate_receipt_requires_the_exact_target_and_a_validated_idr() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.request(4_000_000);
        for (actual, idr) in [(4_000_000, false), (2_000_000, true), (0, true)] {
            assert_eq!(rate.confirm(4_000_000, actual, idr, 10, start), Err(()));
            assert_eq!(rate.applied, 8_000_000);
            assert_eq!(rate.last, None);
            assert_eq!(rate.due(10, start, true), Some(4_000_000));
        }
        // A blocked dwell passes only the old target into native code. A reply
        // claiming the not-yet-admitted proposal cannot bypass that decision.
        assert_eq!(rate.confirm(8_000_000, 4_000_000, true, 10, start), Err(()));
    }

    #[test]
    fn native_targets_preserve_the_opening_ceiling_and_invalid_requests_preserve_intent() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
        for invalid in [0, 9_999, 8_000_001, 200_000_001, u32::MAX] {
            assert_eq!(rate.request_with_ceiling(invalid, 8_000_000), Err(()));
            assert_eq!(rate.due(10, start, true), Some(4_000_000));
            assert_eq!(rate.applied_bitrate(), 8_000_000);
        }
        rate.confirm(4_000_000, 4_000_000, true, 10, start).unwrap();
        // The opening limit, not the current rate, admits a later recovery
        // toward full quality; it cannot skip either original clock's dwell.
        rate.request_with_ceiling(8_000_000, 8_000_000).unwrap();
        assert_eq!(rate.due(2_000_009, start + DWELL, true), None);
        assert_eq!(rate.due(2_000_010, start + DWELL, true), Some(8_000_000));
        assert_eq!(rate.applied_bitrate(), 4_000_000);
        for invalid_ceiling in [0, 9_999, 200_000_001, u32::MAX] {
            assert_eq!(rate.request_with_ceiling(10_000, invalid_ceiling), Err(()));
            assert_eq!(rate.due(2_000_010, start + DWELL, true), Some(8_000_000));
        }
    }

    #[test]
    fn a_legacy_capture_cancels_pending_rate_work_without_changing_the_applied_receipt() {
        let start = Instant::now();
        let mut rate = Rate::new(8_000_000);
        rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
        assert_eq!(rate.due(10, start, true), Some(4_000_000));
        rate.request(rate.applied_bitrate());
        assert_eq!(rate.due(10, start, true), None);
        assert_eq!(rate.applied_bitrate(), 8_000_000);
        rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
        rate.confirm(4_000_000, 4_000_000, true, 10, start).unwrap();
        rate.request_with_ceiling(2_000_000, 8_000_000).unwrap();
        rate.request(rate.applied_bitrate());
        assert_eq!(rate.due(2_000_010, start + DWELL, true), None);
        assert_eq!(rate.applied_bitrate(), 4_000_000);
    }
}
