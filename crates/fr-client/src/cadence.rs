#![forbid(unsafe_code)]
//! Bounded, content-free cadence of one viewer's compositor submissions: the gap
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
    buckets: [u64; BUCKETS],
    gaps: u64,
    // Even u64::MAX observations of u64::MAX microseconds fit in u128.
    total_us: u128,
    max_us: u64,
    hitches: u64,
    stalls: u64,
}

impl Default for FrameCadence {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
            gaps: 0,
            total_us: 0,
            max_us: 0,
            hitches: 0,
            stalls: 0,
        }
    }
}

impl FrameCadence {
    /// Record one gap without allocating or retaining individual observations.
    /// At the u64 sample-count ceiling the complete original prefix is retained.
    pub fn record(&mut self, gap_us: u64) {
        if self.gaps == u64::MAX {
            return;
        }
        let index = usize::try_from(gap_us / BUCKET_US)
            .unwrap_or(usize::MAX)
            .min(BUCKETS - 1);
        self.buckets[index] += 1;
        self.gaps += 1;
        self.total_us += u128::from(gap_us);
        self.max_us = self.max_us.max(gap_us);
        if gap_us > HITCH_US {
            self.hitches += 1;
        }
        if gap_us > STALL_US {
            self.stalls += 1;
        }
    }

    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Upper bound, in whole milliseconds, of the `permille`/1000 quantile:
    /// its bucket's upper edge, or the exact maximum past the last edge.
    pub fn quantile_upper_ms(&self, permille: u64) -> Option<u64> {
        let total = self.gaps();
        if total == 0 || permille > 1000 {
            return None;
        }
        let rank = (u128::from(total) * u128::from(permille))
            .div_ceil(1000)
            .max(1);
        let mut seen = 0;
        for (index, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if u128::from(seen) >= rank {
                return Some(if index == BUCKETS - 1 {
                    self.max_us.div_ceil(BUCKET_US)
                } else {
                    u64::try_from(index + 1).unwrap_or(u64::MAX)
                });
            }
        }
        None
    }

    /// The longest recorded gap in microseconds, without histogram rounding.
    pub fn max_us(&self) -> Option<u64> {
        (self.gaps() > 0).then_some(self.max_us)
    }

    /// The longest gap, rounded up to whole milliseconds.
    pub fn max_ms(&self) -> Option<u64> {
        (self.gaps() > 0).then(|| self.max_us.div_ceil(BUCKET_US))
    }

    /// Mean submitted pictures per second over the recorded gaps, in tenths.
    pub fn mean_per_second_tenths(&self) -> Option<u64> {
        (self.total_us > 0).then(|| {
            u64::try_from(u128::from(self.gaps) * 10_000_000 / self.total_us).unwrap_or(u64::MAX)
        })
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

    #[test]
    fn quantiles_bound_exact_nearest_ranks_at_every_bucket_edge() {
        let mut values = vec![0, 1, 999, 1_000, 1_001, 249_999, 250_000, 250_001];
        values.extend((0..251).flat_map(|ms| [ms * 1_000, ms * 1_000 + 999]));
        values.extend([700_001, 1_200_000, u64::MAX]);
        let mut cadence = FrameCadence::default();
        for &value in &values {
            cadence.record(value);
        }
        values.sort_unstable();
        for permille in 0..=1_000 {
            let rank = (values.len() * permille).div_ceil(1_000).max(1);
            let exact_us = values[rank - 1];
            let upper_ms = cadence
                .quantile_upper_ms(u64::try_from(permille).unwrap())
                .unwrap();
            assert!(u128::from(upper_ms) * 1_000 >= u128::from(exact_us));
            if exact_us < 250_000 {
                assert!(u128::from(upper_ms) * 1_000 <= u128::from(exact_us) + 1_000);
            } else {
                assert_eq!(upper_ms, u64::MAX.div_ceil(1_000));
            }
        }
        assert_eq!(cadence.max_us(), Some(u64::MAX));
    }

    #[test]
    fn a_long_session_keeps_one_fixed_summary_including_rare_stalls() {
        let mut cadence = FrameCadence::default();
        // More than one day at 60 fps, with infrequent genuine long stalls.
        for index in 0..5_184_001 {
            cadence.record(if index % 1_000_000 == 0 {
                900_123
            } else {
                16_667
            });
        }
        assert_eq!(cadence.gaps(), 5_184_001);
        assert_eq!(cadence.quantile_upper_ms(990), Some(17));
        assert_eq!(cadence.quantile_upper_ms(1000), Some(901));
        assert_eq!(cadence.max_us(), Some(900_123));
        assert_eq!((cadence.hitches(), cadence.stalls()), (6, 6));
        assert!(size_of::<FrameCadence>() <= 2_080);
    }

    #[test]
    fn accumulated_duration_does_not_saturate_into_a_false_mean() {
        let mut cadence = FrameCadence::default();
        cadence.record(u64::MAX);
        cadence.record(u64::MAX);
        cadence.record(1);
        assert_eq!(cadence.total_us, u128::from(u64::MAX) * 2 + 1);
        assert_eq!(cadence.mean_per_second_tenths(), Some(0));
        assert_eq!(cadence.max_us(), Some(u64::MAX));
        assert_eq!(cadence.stalls(), 2);
    }

    #[test]
    fn full_sample_count_freezes_the_original_prefix_without_wrapping() {
        let mut cadence = FrameCadence::default();
        cadence.buckets[BUCKETS - 1] = u64::MAX - 1;
        cadence.gaps = u64::MAX - 1;
        cadence.total_us = u128::from(u64::MAX - 1) * u128::from(u64::MAX);
        cadence.max_us = u64::MAX;
        cadence.hitches = u64::MAX - 1;
        cadence.stalls = u64::MAX - 1;
        cadence.record(u64::MAX);
        assert_eq!(cadence.gaps(), u64::MAX);
        assert_eq!(cadence.total_us, u128::from(u64::MAX).pow(2));
        assert_eq!(cadence.hitches(), u64::MAX);
        assert_eq!(cadence.stalls(), u64::MAX);
        assert_eq!(
            cadence.quantile_upper_ms(999),
            Some(u64::MAX.div_ceil(1_000))
        );
        assert_eq!(cadence.mean_per_second_tenths(), Some(0));
        let before = cadence.clone();
        cadence.record(0);
        cadence.record(u64::MAX);
        assert_eq!(cadence, before);
    }

    #[test]
    fn zero_duration_samples_keep_mean_unknown_and_thresholds_are_exclusive() {
        let mut cadence = FrameCadence::default();
        cadence.record(0);
        assert_eq!(cadence.mean_per_second_tenths(), None);
        assert_eq!(cadence.max_us(), Some(0));
        cadence.record(HITCH_US);
        cadence.record(HITCH_US + 1);
        cadence.record(STALL_US);
        cadence.record(STALL_US + 1);
        assert_eq!((cadence.hitches(), cadence.stalls()), (3, 1));
        assert_eq!(cadence.gaps(), 5);
    }
}
