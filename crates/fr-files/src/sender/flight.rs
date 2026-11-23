//! Fixed accounting for admitted records on the original file channel.
//! Only the original channel's retained-record count retires its acknowledged prefix.
//! Payload remains in transport; elapsed time, an incomplete record's ACKs and
//! a new transfer never reclaim a slot or replace an original deadline.

const MAX_RECORDS: usize = 8;
const MAX_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy)]
struct Record {
    bytes: usize,
    deadline: u64,
}

#[derive(Default)]
pub(super) struct Flight {
    records: [Option<Record>; MAX_RECORDS],
}
impl Flight {
    pub(super) fn is_empty(&self) -> bool {
        self.records.iter().all(Option::is_none)
    }
    pub(super) fn permits(&self, bytes: usize) -> bool {
        let used: usize = self.records.iter().flatten().map(|r| r.bytes).sum();
        bytes > 0
            && self.records.iter().any(Option::is_none)
            && used.checked_add(bytes).is_some_and(|n| n <= MAX_BYTES)
    }
    /// Call only after whole-record admission, using serialized bytes rather
    /// than the separate payload/overhead charge used by the rate bucket.
    pub(super) fn record(&mut self, bytes: usize, deadline: u64) -> bool {
        if !self.permits(bytes) {
            return false;
        }
        let slot = self.records.iter_mut().find(|r| r.is_none()).unwrap();
        *slot = Some(Record { bytes, deadline });
        true
    }
    pub(super) fn expired(&self, now: u64) -> bool {
        self.records.iter().flatten().any(|r| now >= r.deadline)
    }
    /// The exclusive channel owner supplies the current retained count AFTER
    /// checking the original connection and lane. Transport retires whole
    /// records in order, so only this ledger's oldest prefix can be released.
    /// A larger count is an ownership mismatch; leave every charge intact.
    pub(super) fn reconcile(&mut self, retained: usize) -> bool {
        let charged = self.records.iter().flatten().count();
        let Some(completed) = charged.checked_sub(retained) else {
            return false;
        };
        self.records.rotate_left(completed);
        self.records[retained..].fill(None);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_record_kinds_consume_the_fixed_eight_slots() {
        let mut f = Flight::default();
        for (index, bytes) in [80, 16_384, 16_384, 16_384, 16_384, 16_384, 16_384, 128]
            .into_iter()
            .enumerate()
        {
            assert!(f.record(bytes, 100 + index as u64));
        }
        assert!(!f.permits(1));
        assert!(!f.record(1, 1_000));
        assert!(!f.is_empty());
    }

    #[test]
    fn serialized_bytes_have_an_independent_exact_cap() {
        let mut f = Flight::default();
        assert!(f.record(MAX_BYTES - 1, 100));
        assert!(!f.record(2, 200));
        assert!(f.record(1, 300));
        assert!(!f.permits(1));
        assert!(!f.expired(99));
        assert!(f.expired(100));
    }

    #[test]
    fn invalid_lengths_never_allocate_or_consume_capacity() {
        let mut f = Flight::default();
        for bytes in [0, MAX_BYTES + 1, usize::MAX] {
            assert!(!f.record(bytes, 10));
            assert!(f.is_empty());
        }
        assert!(f.record(MAX_BYTES, 20));
        assert!(!f.permits(usize::MAX));
    }

    #[test]
    fn later_admissions_cannot_renew_any_earlier_deadline() {
        let mut f = Flight::default();
        assert!(f.record(80, 10));
        assert!(f.record(4_096, 200));
        assert!(f.record(128, 300));
        assert!(!f.expired(9));
        assert!(f.expired(10));
        assert!(f.expired(299));
        assert!(!f.is_empty());
    }

    #[test]
    fn deadline_order_is_not_assumed() {
        let mut f = Flight::default();
        assert!(f.record(80, 300));
        assert!(f.record(4_096, 20));
        assert!(!f.expired(19));
        assert!(f.expired(20));
    }

    #[test]
    fn only_an_exact_native_retained_count_reclaims_the_original_flight() {
        let mut f = Flight::default();
        for _ in 0..MAX_RECORDS {
            assert!(f.record(128, 100));
        }
        assert!(f.expired(100));
        assert!(!f.permits(1));
        assert!(f.reconcile(0));
        assert!(f.is_empty());
        assert!(!f.expired(u64::MAX));
        assert!(f.record(16_384, 1_000));
        assert!(!f.expired(999));
        assert!(f.expired(1_000));
    }

    #[test]
    fn acknowledged_prefix_releases_capacity_and_only_its_original_deadline() {
        let mut f = Flight::default();
        assert!(f.record(40_000, 10));
        assert!(f.record(40_000, 50));
        assert!(f.record(40_000, 30));
        assert!(!f.permits(20_000));
        assert!(f.expired(10));
        assert!(f.reconcile(2));
        assert!(f.permits(20_000));
        assert!(!f.expired(10));
        assert!(!f.expired(29));
        assert!(f.expired(30));
        assert!(f.record(20_000, 100));
        assert!(f.reconcile(2));
        assert!(f.expired(30));
        assert!(f.reconcile(1));
        assert!(!f.expired(99));
        assert!(f.expired(100));
    }

    #[test]
    fn unchanged_and_impossible_counts_preserve_all_charges() {
        let mut f = Flight::default();
        for index in 0..MAX_RECORDS {
            assert!(f.record(100, 10 + index as u64));
        }
        assert!(f.reconcile(MAX_RECORDS));
        assert!(!f.reconcile(MAX_RECORDS + 1));
        assert!(!f.reconcile(usize::MAX));
        assert!(!f.permits(1));
        assert!(f.expired(10));
        assert!(f.reconcile(7));
        assert!(!f.expired(10));
        assert!(f.expired(11));
        assert!(f.record(100, 100));
        assert!(!f.permits(1));
    }
}
