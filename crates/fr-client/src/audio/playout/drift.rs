//! Slow phase correction from admitted source samples and the LOCAL device clock.
//! One scalar lower-envelope sample per second rejects positive arrival jitter;
//! no arrival FIFO, source-clock inference from packet count, or growing history.
//!
//! Shared by periodic playout and native graph-demand owners. Both clocks use
//! 48 kHz per-channel samples; local observation time uses monotonic microseconds.
//! This estimator neither grants output nor replaces a native demand, original
//! packet deadline, bounded queue, or permission check. A stopped or changed
//! device requires a new owner and a fresh estimator, never relabeled counters.
use fr_core::audio::OPUS_SAMPLE_RATE;
use fr_media::audio::clock::MAX_CORRECTION_PPM;

const PERIOD_US: u64 = 1_000_000;
const DEAD_BAND_SAMPLES: i128 = 96; // 2 ms, not a new retention budget.
const SLEW_PPM: i32 = 100;
const SETTLE_SECONDS: i128 = 10;

/// One planned output interval with private fractional rounding credit. Keep it
/// with the outstanding decode and commit it once after native admission.
#[derive(Clone, Copy)]
pub struct Span {
    pub(super) samples: u32,
    fraction: i64,
}
impl Span {
    /// Actual output frames for this one planned nominal source frame. The
    /// rounding credit stays private until the originating owner commits it.
    pub const fn samples(self) -> u32 {
        self.samples
    }
}

/// Bounded local-device phase correction for one uninterrupted audio epoch.
/// Begin with `Default` and retain this value alongside the native output owner.
/// Source timestamps and device positions remain distinct throughout its life.
#[derive(Default)]
pub struct Drift {
    anchor: Option<(u64, u64)>, // Next source sample and its scheduled device sample.
    latest_source: Option<u64>,
    window: Option<u64>,
    earliest_phase: Option<i128>,
    ppm: i32,
    fraction: i64,
}
impl Drift {
    /// Current correction, clamped to the shared ±1000 ppm output limit.
    pub const fn ppm(&self) -> i32 {
        self.ppm
    }
    /// Only uniquely admitted, forward-moving source samples are observations.
    /// Worker-inbox delay is part of arrival jitter, never a new source clock.
    /// `device` comes from actual native sample progress, never submitted packet
    /// counts. `now` is fresh local monotonic microseconds; `target` is the
    /// negotiated jitter target in samples. The containing owner rejects clock
    /// regression or device discontinuity before calling this method.
    pub fn observe(&mut self, source: u64, device: u64, now: u64, target: u64) {
        let Some((source_anchor, device_anchor)) = self.anchor else {
            return;
        };
        if self.latest_source.is_some_and(|latest| source <= latest) {
            return;
        }
        self.latest_source = Some(source);
        let predicted = i128::from(device_anchor) + i128::from(source) - i128::from(source_anchor);
        let phase = i128::from(device) + i128::from(target) - predicted;
        let earliest = self.earliest_phase.get_or_insert(phase);
        *earliest = (*earliest).min(phase);
        let started = *self.window.get_or_insert(now);
        if now.saturating_sub(started) < PERIOD_US {
            return;
        }
        // Proportional phase servo with a 10-second time constant and at most
        // 100 ppm slew per observation window. Large jumps cannot be hidden:
        // all existing packet-age, device-slot and generation fences still run.
        let desired = if earliest.abs() <= DEAD_BAND_SAMPLES {
            0
        } else {
            i32::try_from(
                (*earliest * 1_000_000 / (i128::from(OPUS_SAMPLE_RATE) * SETTLE_SECONDS)).clamp(
                    -i128::from(MAX_CORRECTION_PPM),
                    i128::from(MAX_CORRECTION_PPM),
                ),
            )
            .expect("correction is clamped to 1000 ppm")
        };
        self.ppm += (desired - self.ppm).clamp(-SLEW_PPM, SLEW_PPM);
        self.window = Some(now);
        self.earliest_phase = None;
    }
    /// Read-only plan. A pending decode retains this exact slot, even when new
    /// arrivals alter the estimate. Polling/backpressure cannot spend rounding
    /// credit or extend the pending slot. Commit only after actual submission.
    /// `nominal` must be the validated configuration's per-channel sample count;
    /// output adapters independently enforce `clock::valid_sample_count`.
    ///
    /// # Panics
    ///
    /// Panics if an invalid nominal count overflows the corrected `u32` count.
    /// Negotiated audio frame sizes cannot reach that bound.
    pub fn plan(&self, nominal: u32) -> Span {
        let scaled = self.fraction + i64::from(nominal) * i64::from(self.ppm);
        let adjustment = scaled / 1_000_000;
        Span {
            samples: u32::try_from(i64::from(nominal) + adjustment)
                .expect("negotiated frame and bounded correction"),
            fraction: scaled % 1_000_000,
        }
    }
    /// Commit the exact span accepted by this owner's bounded native output.
    /// `source_next` advances by the NOMINAL decoded frame count; `device_next`
    /// advances by `span.samples()`. These are scheduling anchors, never proof
    /// that an application received sound. Do not commit on a rejected or unknown
    /// submission, reuse a span, or use a span planned by another estimator.
    pub fn submitted(&mut self, span: Span, source_next: u64, device_next: u64) {
        self.fraction = span.fraction;
        self.anchor = Some((source_next, device_next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_samples_are_spent_once_per_output_not_once_per_poll() {
        for ppm in [-1000, -501, -1, 0, 1, 501, 1000] {
            for nominal in [240, 480, 960, 1920, 4800] {
                let mut drift = Drift {
                    ppm,
                    ..Drift::default()
                };
                let mut total = 0_u64;
                for i in 0..1000 {
                    let span = drift.plan(nominal);
                    for _ in 0..4 {
                        let again = drift.plan(nominal);
                        assert_eq!(again.samples, span.samples);
                        assert_eq!(again.fraction, span.fraction);
                    }
                    assert!(fr_media::audio::clock::valid_sample_count(
                        nominal as usize,
                        span.samples as usize
                    ));
                    total += u64::from(span.samples);
                    drift.submitted(span, i * u64::from(nominal), total);
                }
                assert_eq!(
                    i64::try_from(total).unwrap(),
                    i64::from(nominal) * 1000 + i64::from(nominal) * i64::from(ppm) / 1000
                );
            }
        }
    }
    #[test]
    fn arrival_bursts_duplicates_and_reordering_cannot_accelerate_the_servo() {
        let mut drift = Drift::default();
        drift.submitted(drift.plan(480), 0, 960);
        // Sustained 20 ms phase error, yet no estimate before a full second.
        for source in 1..=1000 {
            drift.observe(source, source + 960, 100, 960);
            assert_eq!(drift.ppm(), 0);
        }
        drift.observe(1001, 1961, PERIOD_US + 100, 960);
        assert_eq!(drift.ppm(), SLEW_PPM);
        for _ in 0..1000 {
            drift.observe(1001, 100_000, 50 * PERIOD_US, 960);
            drift.observe(1000, 100_000, 50 * PERIOD_US, 960);
        }
        assert_eq!(drift.ppm(), SLEW_PPM);
        // A later forward observation gets only ONE step, not catch-up steps
        // for the elapsed wall time. Long stalls remain the playout owner's fault.
        drift.observe(1002, 1962, 50 * PERIOD_US, 960);
        assert_eq!(drift.ppm(), 2 * SLEW_PPM);
    }
    #[test]
    fn full_width_sample_origins_do_not_wrap_or_change_correction_direction() {
        for source in [0, u64::MAX - 100_000] {
            for device in [0, u64::MAX - 100_000] {
                let mut drift = Drift::default();
                drift.submitted(drift.plan(480), source, device);
                drift.observe(source + 1, device + 961, 0, 0);
                drift.observe(source + 2, device + 962, PERIOD_US, 0);
                assert_eq!(drift.ppm(), SLEW_PPM);
            }
        }
    }
}
