//! Bounded, opportunistic batching; never an acknowledgment witness.
//!
//! Already admitted records may join for a fixed ten milliseconds after the
//! first native prefix. There is no batching delay: the first record is sent
//! immediately. Critical batches admit at most four records this way, so a
//! burst of clock, renewal or input feedback need not wait one RTT per record
//! while a busy producer cannot pin an unbounded flight. An initially frozen
//! batch may already be larger; its original membership is not changed.
//!
//! Bulk batching and the connection's separate byte/count budgets are unchanged.
//! Every original delivery deadline remains in force. Credit is released only
//! after BOTH native pending and retransmission storage are proven empty.

const FILL_MICROS: u64 = 10_000;
const CRITICAL_FILL_RECORDS: usize = 4;

pub(super) struct SendEpoch {
    pub(super) bytes: usize,
    pub(super) records: usize,
    // Keep the first-prefix time, not a sliding last-admission time.
    fill_started: u64,
    bulk: bool,
}

impl SendEpoch {
    pub(super) const fn new(bytes: usize, records: usize, bulk: bool, now: u64) -> Self {
        Self {
            bytes,
            records,
            fill_started: now,
            bulk,
        }
    }

    /// Called only AFTER the connection's byte/count, allocation, authority
    /// and deadline gates accepted a whole record. Failure leaves that record
    /// in the next batch, with its original charge and delivery deadline.
    /// This never stages native bytes or reclaims credit.
    pub(super) fn try_join(&mut self, bytes: usize, now: u64) -> bool {
        let Some(elapsed) = now.checked_sub(self.fill_started) else {
            return false;
        };
        if elapsed >= FILL_MICROS || (!self.bulk && self.records >= CRITICAL_FILL_RECORDS) {
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
    fn critical_bursts_join_without_waiting_for_the_first_records_ack() {
        let mut epoch = SendEpoch::new(64, 1, false, 100_000);
        for now in [101_000, 105_000, 109_999] {
            assert!(epoch.try_join(64, now));
        }
        assert_eq!((epoch.bytes, epoch.records), (256, CRITICAL_FILL_RECORDS));
        // This is still the original flight; no ACK or credit release was
        // fabricated to admit these already-budgeted records to it.
        assert!(!epoch.try_join(64, 109_999));
        assert_eq!((epoch.bytes, epoch.records), (256, CRITICAL_FILL_RECORDS));
    }

    #[test]
    fn critical_fill_has_an_exclusive_fixed_deadline_not_a_sliding_one() {
        let mut epoch = SendEpoch::new(64, 1, false, 7);
        assert!(epoch.try_join(64, 10_006));
        for now in [10_007, 10_008, 20_000, u64::MAX] {
            assert!(!epoch.try_join(64, now));
            assert_eq!((epoch.bytes, epoch.records), (128, 2));
        }
    }

    #[test]
    fn an_initially_full_critical_batch_keeps_its_membership() {
        for records in [CRITICAL_FILL_RECORDS, 16, usize::MAX] {
            let mut epoch = SendEpoch::new(512, records, false, 100);
            assert!(!epoch.try_join(64, 100));
            assert_eq!((epoch.bytes, epoch.records), (512, records));
        }
    }

    #[test]
    fn rejected_critical_joins_do_not_spend_slots_or_restart_time() {
        let mut epoch = SendEpoch::new(usize::MAX - 10, 1, false, 100);
        assert!(!epoch.try_join(1, 99));
        assert!(!epoch.try_join(11, 101));
        assert_eq!((epoch.bytes, epoch.records), (usize::MAX - 10, 1));
        assert!(epoch.try_join(10, 10_099));
        assert_eq!((epoch.bytes, epoch.records), (usize::MAX, 2));
        assert!(!epoch.try_join(0, 10_100));
        assert_eq!((epoch.bytes, epoch.records), (usize::MAX, 2));
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
