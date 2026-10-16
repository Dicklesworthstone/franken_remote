//! Bounded, opportunistic bulk batching; never an acknowledgment witness.
//!
//! Critical streams keep their closed batches. Bulk streams may add already
//! admitted records for a fixed ten milliseconds after the first native prefix.
//! There is no batching delay: the first record is sent immediately. This lets
//! a producer serviced on successive scheduler turns use the existing retained
//! budget rather than waiting one RTT for every small record. The owner still
//! enforces every original delivery deadline and releases credit only after
//! BOTH native pending and retransmission storage are proven empty.

const BULK_FILL_MICROS: u64 = 10_000;

pub(super) struct SendEpoch {
    pub(super) bytes: usize,
    pub(super) records: usize,
    // None for critical streams. Keep the start, not a sliding last-admission
    // time; a busy producer cannot indefinitely keep a batch open.
    fill_started: Option<u64>,
}

impl SendEpoch {
    pub(super) const fn new(bytes: usize, records: usize, bulk: bool, now: u64) -> Self {
        Self {
            bytes,
            records,
            fill_started: if bulk { Some(now) } else { None },
        }
    }

    /// Called only AFTER the connection's byte/count, allocation, authority
    /// and deadline gates accepted a whole record. Failure leaves that record
    /// in the next batch, with its original charge and delivery deadline.
    /// This never stages native bytes or reclaims credit.
    pub(super) fn try_join(&mut self, bytes: usize, now: u64) -> bool {
        let Some(elapsed) = self.fill_started.and_then(|start| now.checked_sub(start)) else {
            return false;
        };
        if elapsed >= BULK_FILL_MICROS {
            return false;
        }
        let Some(total_bytes) = self.bytes.checked_add(bytes) else {
            return false;
        };
        let Some(total_records) = self.records.checked_add(1) else {
            return false;
        };
        self.bytes = total_bytes;
        self.records = total_records;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staggered_bulk_records_share_the_existing_flight() {
        let mut epoch = SendEpoch::new(128, 1, true, 100_000);
        for turn in 1..10 {
            assert!(epoch.try_join(8192, 100_000 + turn * 1000));
        }
        assert_eq!((epoch.bytes, epoch.records), (128 + 9 * 8192, 10));
        // The first record's ACK need not be awaited to stage these records.
        // The next turn belongs to the next batch even if no ACK has arrived.
        assert!(!epoch.try_join(8192, 110_000));
        assert_eq!((epoch.bytes, epoch.records), (128 + 9 * 8192, 10));
    }

    #[test]
    fn critical_batches_never_admit_later_records() {
        let mut epoch = SendEpoch::new(64, 1, false, 100);
        for now in [99, 100, 101, 1099, 10_099, 10_100, u64::MAX] {
            assert!(!epoch.try_join(64, now));
            assert_eq!((epoch.bytes, epoch.records), (64, 1));
        }
    }

    #[test]
    fn continuous_admission_cannot_slide_the_fill_window() {
        let mut epoch = SendEpoch::new(24, 1, true, 7);
        for now in 7..10_007 {
            assert!(epoch.try_join(24, now));
        }
        assert_eq!((epoch.bytes, epoch.records), (24 * 10_001, 10_001));
        for now in [10_007, 10_008, 20_000, u64::MAX] {
            assert!(!epoch.try_join(24, now));
            assert_eq!((epoch.bytes, epoch.records), (24 * 10_001, 10_001));
        }
    }

    #[test]
    fn clock_regression_and_overflow_never_open_a_batch() {
        let mut epoch = SendEpoch::new(24, 1, true, u64::MAX - 20_000);
        assert!(!epoch.try_join(24, u64::MAX - 20_001));
        assert!(epoch.try_join(24, u64::MAX - 10_001));
        assert!(!epoch.try_join(24, u64::MAX - 10_000));
        assert!(!epoch.try_join(24, u64::MAX));
        assert_eq!((epoch.bytes, epoch.records), (48, 2));
        let mut at_end = SendEpoch::new(24, 1, true, u64::MAX);
        assert!(at_end.try_join(24, u64::MAX));
        assert!(!at_end.try_join(24, 0));
    }

    #[test]
    fn accounting_overflow_is_atomic_and_does_not_renew_the_window() {
        let mut bytes = SendEpoch::new(usize::MAX - 10, 2, true, 0);
        assert!(!bytes.try_join(11, 1));
        assert_eq!((bytes.bytes, bytes.records), (usize::MAX - 10, 2));
        assert!(bytes.try_join(10, 9999));
        assert!(!bytes.try_join(0, 10_000));
        let mut records = SendEpoch::new(10, usize::MAX, true, 0);
        assert!(!records.try_join(20, 1));
        assert_eq!((records.bytes, records.records), (10, usize::MAX));
    }
}
