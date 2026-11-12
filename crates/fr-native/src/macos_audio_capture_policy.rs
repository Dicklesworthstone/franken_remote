#![forbid(unsafe_code)]
//! Actual host-audio packing policy, shared with portable tests. The native
//! boundary supplies 48 kHz floating-point PCM and the ORIGINAL source-age
//! deadline; neither a read nor a completed frame renews that deadline.
use std::collections::VecDeque;

pub const MAX_CHUNK_SAMPLES: usize = 4_800;
pub const MAX_CHUNK_VALUES: usize = MAX_CHUNK_SAMPLES * 2;
pub const MAX_FRAME_VALUES: usize = 960 * 2;
pub const MAX_BATCH_FRAMES: usize = 4;
pub const SOURCE_AGE_NS: u64 = 100_000_000;
pub const READ_BUDGET_NS: u64 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Unavailable,
    Permission,
    Allocation,
    Native,
    Closed,
    Expired,
    Clock,
    BufferLimit,
    Format,
    Discontinuity,
    Busy,
    Cleanup,
    Suspended,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "macos-audio-capture: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Fixed scalar metadata, never a pointer into a sample buffer. The timestamp
/// is the first source sample, rounded down in its actual 48 kHz clock domain.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stamp {
    pub timestamp_samples: u64,
    pub valid_until_ns: u64,
    pub frames: u32,
    pub channels: u32,
}

pub struct Packed {
    pub samples: Box<[i16]>,
    pub timestamp_samples: u64,
    pub valid_until_ns: u64,
}

/// A single partial frame plus four complete frames. No output can grow with
/// a callback's declared size, elapsed time, silent source or slow consumer.
pub struct Packer {
    channels: usize,
    frame_samples: usize,
    partial: Box<[i16; MAX_FRAME_VALUES]>,
    filled: usize,
    partial_start: u64,
    partial_until: u64,
    expected: Option<u64>,
    ready: VecDeque<Packed>,
    gaps: u32,
}
impl Packer {
    pub fn new(channels: u8, frame_samples: u16) -> Result<Self, Error> {
        if !matches!(channels, 1 | 2) || !matches!(frame_samples, 480 | 960) {
            return Err(Error::Configuration);
        }
        Ok(Self {
            channels: usize::from(channels),
            frame_samples: usize::from(frame_samples),
            partial: Box::new([0; MAX_FRAME_VALUES]),
            filled: 0,
            partial_start: 0,
            partial_until: 0,
            expected: None,
            ready: VecDeque::with_capacity(MAX_BATCH_FRAMES),
            gaps: 0,
        })
    }

    pub const fn gaps(&self) -> u32 {
        self.gaps
    }

    fn gap(&mut self) {
        self.gaps = self.gaps.saturating_add(1);
    }

    fn discard_partial(&mut self) {
        if self.filled != 0 {
            self.partial[..self.filled].fill(0);
            self.filled = 0;
            self.gap();
        }
    }

    /// Source timestamps can differ by one sample after the native rational
    /// timestamp is rounded down. Preserve the first sample's spacing in that
    /// case; a larger forward hole is explicit, and reversal is terminal.
    fn start(&mut self, timestamp: u64) -> Result<u64, Error> {
        match self.expected {
            Some(expected) if timestamp.abs_diff(expected) <= 1 => Ok(expected),
            Some(expected) if timestamp < expected => Err(Error::Discontinuity),
            Some(_) => {
                self.discard_partial();
                self.gap();
                Ok(timestamp)
            }
            None => Ok(timestamp),
        }
    }

    pub fn push(&mut self, stamp: Stamp, pcm: &[f32], now_ns: u64) -> Result<(), Error> {
        let frames = usize::try_from(stamp.frames).map_err(|_| Error::BufferLimit)?;
        if frames == 0
            || frames > MAX_CHUNK_SAMPLES
            || usize::try_from(stamp.channels).ok() != Some(self.channels)
            || frames.checked_mul(self.channels) != Some(pcm.len())
            || pcm.len() > MAX_CHUNK_VALUES
            || stamp.valid_until_ns == 0
            || stamp.valid_until_ns.saturating_sub(now_ns) > SOURCE_AGE_NS
        {
            return Err(Error::Format);
        }
        // Validate all values before mutating the accumulator. NaN and infinity
        // never reach a float-to-integer conversion, an allocation or libopus.
        if pcm.iter().any(|value| !value.is_finite()) {
            return Err(Error::Format);
        }
        let start = self.start(stamp.timestamp_samples)?;
        self.expected = Some(
            start
                .checked_add(u64::from(stamp.frames))
                .ok_or(Error::Clock)?,
        );
        if self.filled != 0 && now_ns >= self.partial_until {
            self.discard_partial();
        }
        if now_ns >= stamp.valid_until_ns {
            self.discard_partial();
            self.gap();
            return Ok(());
        }
        let frame_values = self.frame_samples * self.channels;
        for (index, values) in pcm.chunks_exact(self.channels).enumerate() {
            if self.filled == 0 {
                self.partial_start = start
                    .checked_add(u64::try_from(index).map_err(|_| Error::Clock)?)
                    .ok_or(Error::Clock)?;
                self.partial_until = stamp.valid_until_ns;
            } else {
                self.partial_until = self.partial_until.min(stamp.valid_until_ns);
            }
            for value in values {
                self.partial[self.filled] = quantize(*value);
                self.filled += 1;
            }
            if self.filled == frame_values {
                if self.ready.len() == MAX_BATCH_FRAMES {
                    self.ready.pop_front();
                    self.gap();
                }
                self.ready.push_back(Packed {
                    samples: self.partial[..frame_values].into(),
                    timestamp_samples: self.partial_start,
                    valid_until_ns: self.partial_until,
                });
                self.partial[..frame_values].fill(0);
                self.filled = 0;
            }
        }
        Ok(())
    }

    pub fn take(&mut self, now_ns: u64) -> Vec<Packed> {
        let mut frames = Vec::with_capacity(MAX_BATCH_FRAMES);
        while let Some(frame) = self.ready.pop_front() {
            if now_ns < frame.valid_until_ns {
                frames.push(frame);
            } else {
                self.gap();
            }
        }
        frames
    }

    pub fn clear(&mut self) {
        self.ready.clear();
        self.partial.fill(0);
        self.filled = 0;
    }
}

#[allow(clippy::cast_possible_truncation)] // Finite, explicitly clipped s16 PCM.
fn quantize(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * 32_768.0).clamp(-32_768.0, 32_767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(at: u64, frames: u32, channels: u32, until: u64) -> Stamp {
        Stamp {
            timestamp_samples: at,
            valid_until_ns: until,
            frames,
            channels,
        }
    }

    #[test]
    fn fragmented_stereo_is_packed_without_renewing_the_oldest_sample() {
        let mut packer = Packer::new(2, 480).unwrap();
        packer
            .push(stamp(48_000, 160, 2, 90), &[0.5; 320], 1)
            .unwrap();
        assert!(packer.take(2).is_empty());
        packer
            .push(stamp(48_160, 320, 2, 140), &[-0.5; 640], 3)
            .unwrap();
        let frames = packer.take(4);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_samples, 48_000);
        assert_eq!(frames[0].valid_until_ns, 90);
        assert_eq!(&frames[0].samples[..320], &[16_384; 320]);
        assert_eq!(&frames[0].samples[320..], &[-16_384; 640]);
    }

    #[test]
    fn native_format_lengths_and_nonfinite_values_are_refused_before_output() {
        for channels in [0, 3, u8::MAX] {
            assert!(matches!(
                Packer::new(channels, 480),
                Err(Error::Configuration)
            ));
        }
        for duration in [0, 479, 481, 2_880] {
            assert!(matches!(
                Packer::new(2, duration),
                Err(Error::Configuration)
            ));
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut packer = Packer::new(1, 480).unwrap();
            let mut pcm = [0.0; 480];
            pcm[479] = value;
            assert_eq!(
                packer.push(stamp(0, 480, 1, 100), &pcm, 1),
                Err(Error::Format)
            );
            assert!(packer.take(2).is_empty());
        }
        let mut packer = Packer::new(1, 480).unwrap();
        for original in [
            stamp(0, 480, 2, 100),
            stamp(0, u32::MAX, 1, 100),
            stamp(0, 0, 1, 100),
        ] {
            assert_eq!(packer.push(original, &[0.0; 480], 1), Err(Error::Format));
        }
        assert!(packer.take(2).is_empty());
    }

    #[test]
    fn newest_four_frames_replace_old_raw_work_with_explicit_gaps() {
        let mut packer = Packer::new(1, 480).unwrap();
        packer
            .push(stamp(1_000, 4_800, 1, 100), &vec![0.25; 4_800], 1)
            .unwrap();
        let frames = packer.take(2);
        assert_eq!(frames.len(), MAX_BATCH_FRAMES);
        assert_eq!(frames[0].timestamp_samples, 1_000 + 6 * 480);
        assert_eq!(frames[3].timestamp_samples, 1_000 + 9 * 480);
        assert_eq!(packer.gaps(), 6);
        assert!(packer.take(3).is_empty());
    }

    #[test]
    fn partial_and_complete_expiry_drop_sound_without_retiming_it() {
        let mut packer = Packer::new(1, 480).unwrap();
        packer.push(stamp(0, 240, 1, 20), &[0.5; 240], 1).unwrap();
        packer
            .push(stamp(240, 240, 1, 80), &[0.5; 240], 21)
            .unwrap();
        assert!(packer.take(22).is_empty());
        packer
            .push(stamp(480, 240, 1, 90), &[0.5; 240], 23)
            .unwrap();
        let frames = packer.take(24);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_samples, 240);
        assert_eq!(frames[0].valid_until_ns, 80);
        packer
            .push(stamp(720, 480, 1, 90), &[0.5; 480], 25)
            .unwrap();
        assert!(packer.take(90).is_empty());
        assert_eq!(packer.gaps(), 2);
    }

    #[test]
    fn gaps_remain_explicit_and_reversed_source_time_is_terminal() {
        let mut packer = Packer::new(1, 480).unwrap();
        packer
            .push(stamp(1_000, 240, 1, 100), &[0.0; 240], 1)
            .unwrap();
        packer
            .push(stamp(2_000, 480, 1, 100), &[1.2; 480], 2)
            .unwrap();
        let frames = packer.take(3);
        assert_eq!(frames[0].timestamp_samples, 2_000);
        assert_eq!(frames[0].samples.as_ref(), &[i16::MAX; 480]);
        assert_eq!(packer.gaps(), 2);
        assert_eq!(
            packer.push(stamp(2_000, 480, 1, 100), &[0.0; 480], 4),
            Err(Error::Discontinuity)
        );
    }

    #[test]
    fn one_sample_rounding_does_not_hide_a_real_hole_or_refresh_time() {
        let mut packer = Packer::new(1, 480).unwrap();
        packer
            .push(stamp(10, 240, 1, 100), &[-2.0; 240], 1)
            .unwrap();
        packer
            .push(stamp(249, 240, 1, 110), &[-2.0; 240], 2)
            .unwrap();
        let frames = packer.take(3);
        assert_eq!(frames[0].timestamp_samples, 10);
        assert_eq!(frames[0].valid_until_ns, 100);
        assert_eq!(frames[0].samples.as_ref(), &[i16::MIN; 480]);
        assert_eq!(packer.gaps(), 0);
        assert_eq!(
            packer.push(stamp(u64::MAX, 480, 1, 100), &[0.0; 480], 4),
            Err(Error::Clock)
        );
    }
}
