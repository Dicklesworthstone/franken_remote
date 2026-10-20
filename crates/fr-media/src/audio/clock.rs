//! Small local-clock correction of already decoded PCM, not a wire format.
//! No allocation, new PCM buffer, codec history change or timestamp relabeling.
use super::{AudioMediaError, AudioPcmFrame, MAX_DECODED_SAMPLES};

/// Absolute rate-correction ceiling; fractional samples accumulate across frames.
pub const MAX_CORRECTION_PPM: i32 = 1000;

/// The native sink checks this independently of the scheduling owner. A single
/// frame can differ by one rounding sample beyond the fractional rate bound.
pub fn valid_sample_count(nominal: usize, actual: usize) -> bool {
    nominal > 1
        && nominal <= MAX_DECODED_SAMPLES as usize
        && actual > 1
        && actual <= MAX_DECODED_SAMPLES as usize
        && nominal.abs_diff(actual) <= nominal.div_ceil(1000)
}

impl AudioPcmFrame {
    /// Fit one decoded frame to a bounded local-device slot. Interpolate each
    /// channel independently; preserve both endpoints, epoch and source timestamp.
    /// Identity is bit-exact. This modest linear converter is not a band-limited
    /// resampler and is deliberately restricted to slow clock-rate correction.
    pub fn resample_for_clock(&mut self, output: usize) -> Result<(), AudioMediaError> {
        let input = self.samples_per_channel;
        if !valid_sample_count(input, output) {
            return Err(AudioMediaError::InvalidPayload);
        }
        if input == output {
            return Ok(());
        }
        // Shrinking reads ahead of each write; growing runs backwards and
        // reads behind it. No original sample is overwritten before its use.
        if output < input {
            for at in 0..output {
                self.interpolate_clock_sample(at, input, output);
            }
        } else {
            for at in (0..output).rev() {
                self.interpolate_clock_sample(at, input, output);
            }
        }
        let channels = usize::from(self.channels.count());
        if output < input {
            self.samples[output * channels..input * channels].fill(0);
        }
        self.samples_per_channel = output;
        Ok(())
    }
    fn interpolate_clock_sample(&mut self, at: usize, input: usize, output: usize) {
        // All products are bounded by MAX_DECODED_SAMPLES squared (5,760^2).
        let position = at * (input - 1);
        let denominator = output - 1;
        let first = position / denominator;
        let remainder = position % denominator;
        let second = (first + 1).min(input - 1);
        let channels = usize::from(self.channels.count());
        for channel in 0..channels {
            let a = i64::from(self.samples[first * channels + channel]);
            let b = i64::from(self.samples[second * channels + channel]);
            let value =
                (a * (denominator - remainder) as i64 + b * remainder as i64) / denominator as i64;
            // A convex combination of two i16 samples cannot overflow i16.
            self.samples[at * channels + channel] = value as i16;
        }
    }
}
