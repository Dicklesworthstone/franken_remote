//! Content-free cadence of one viewer's compositor submissions: the gap
//! between consecutive presented pictures within one attempt. A submission is
//! not a displayed refresh, and an unchanged host screen deliberately sends
//! fewer pictures (adaptive capture), so judge smoothness on moving content.

const BUCKET_US: u64 = 1_000;
// 1 ms buckets up to 250 ms; longer gaps share the last bucket.
const BUCKETS: usize = 251;
/// Three pictures at 60 Hz: a hitch a viewer sees.
pub const HITCH_US: u64 = 50_000;
/// A stall: the picture visibly stops.
pub const STALL_US: u64 = 250_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCadence {
    buckets: [u32; BUCKETS],
    total_us: u64,
    max_us: u64,
    hitches: u64,
    stalls: u64,
}

impl Default for FrameCadence {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
            total_us: 0,
            max_us: 0,
            hitches: 0,
            stalls: 0,
        }
    }
}

impl FrameCadence {
    pub fn record(&mut self, gap_us: u64) {
        let index = usize::try_from(gap_us / BUCKET_US)
            .unwrap_or(usize::MAX)
            .min(BUCKETS - 1);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.total_us = self.total_us.saturating_add(gap_us);
        self.max_us = self.max_us.max(gap_us);
        if gap_us > HITCH_US {
            self.hitches += 1;
        }
        if gap_us > STALL_US {
            self.stalls += 1;
        }
    }

    pub fn gaps(&self) -> u64 {
        self.buckets.iter().map(|&n| u64::from(n)).sum()
    }

    /// Upper bound, in whole milliseconds, of the `permille`/1000 quantile:
    /// its bucket's upper edge, or the exact maximum past the last edge.
    pub fn quantile_upper_ms(&self, permille: u64) -> Option<u64> {
        let total = self.gaps();
        if total == 0 || permille > 1000 {
            return None;
        }
        let rank = (total * permille).div_ceil(1000).max(1);
        let mut seen = 0;
        for (index, &n) in self.buckets.iter().enumerate() {
            seen += u64::from(n);
            if seen >= rank {
                return Some(if index == BUCKETS - 1 {
                    self.max_us.div_ceil(BUCKET_US)
                } else {
                    u64::try_from(index + 1).unwrap_or(u64::MAX)
                });
            }
        }
        None
    }

    /// The longest gap, rounded up to whole milliseconds.
    pub fn max_ms(&self) -> Option<u64> {
        (self.gaps() > 0).then(|| self.max_us.div_ceil(BUCKET_US))
    }

    /// Mean submitted pictures per second over the recorded gaps, in tenths.
    pub fn mean_per_second_tenths(&self) -> Option<u64> {
        (self.total_us > 0).then(|| self.gaps().saturating_mul(10_000_000) / self.total_us)
    }

    /// Gaps longer than [`HITCH_US`].
    pub fn hitches(&self) -> u64 {
        self.hitches
    }

    /// Gaps longer than [`STALL_US`].
    pub fn stalls(&self) -> u64 {
        self.stalls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_60_hz_reads_as_17_ms_gaps_and_60_per_second() {
        let mut cadence = FrameCadence::default();
        for _ in 0..600 {
            cadence.record(16_667);
        }
        assert_eq!(cadence.gaps(), 600);
        assert_eq!(cadence.quantile_upper_ms(500), Some(17));
        assert_eq!(cadence.quantile_upper_ms(990), Some(17));
        assert_eq!(cadence.max_ms(), Some(17));
        assert_eq!(cadence.mean_per_second_tenths(), Some(599));
        assert_eq!((cadence.hitches(), cadence.stalls()), (0, 0));
    }

    #[test]
    fn a_rare_long_gap_shows_in_the_tail_and_the_counts_not_the_median() {
        let mut cadence = FrameCadence::default();
        for _ in 0..98 {
            cadence.record(16_667);
        }
        cadence.record(80_000);
        cadence.record(1_200_000);
        assert_eq!(cadence.quantile_upper_ms(500), Some(17));
        // The 99th of 100 gaps is the 80 ms hitch; the 100th, past the last
        // bucket, reports the exact maximum rather than the 251 ms edge.
        assert_eq!(cadence.quantile_upper_ms(990), Some(81));
        assert_eq!(cadence.quantile_upper_ms(1000), Some(1_200));
        assert_eq!(cadence.max_ms(), Some(1_200));
        assert_eq!((cadence.hitches(), cadence.stalls()), (2, 1));
    }

    #[test]
    fn nothing_recorded_claims_nothing() {
        let cadence = FrameCadence::default();
        assert_eq!(cadence.gaps(), 0);
        assert_eq!(cadence.quantile_upper_ms(500), None);
        assert_eq!(cadence.max_ms(), None);
        assert_eq!(cadence.mean_per_second_tenths(), None);
        let mut one = FrameCadence::default();
        one.record(16_000);
        assert_eq!(one.quantile_upper_ms(1001), None);
        // Exactly on an edge rounds into the next bucket's upper edge.
        assert_eq!(one.quantile_upper_ms(500), Some(17));
    }
}
