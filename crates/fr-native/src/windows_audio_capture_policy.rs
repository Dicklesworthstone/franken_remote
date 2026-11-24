#![forbid(unsafe_code)]
//! Native WASAPI packet policy, exercised without a Windows audio device.
//! The endpoint engine supplies the requested 48 kHz s16 PCM stream. Native
//! QPC timestamps, already in 100 ns units, supply immutable source age. The
//! sample timeline uses `GetBuffer`'s stream-relative frames in the accepted
//! WAVEFORMATEX format, not `IAudioClock2`'s unprocessed hardware positions.
use crate::host_audio_capture_policy::{Error, MAX_CHUNK_SAMPLES, SOURCE_AGE_NS, Stamp};

// Values from audioclient.h. The native adapter validates this exact mask.
pub const DISCONTINUITY: u32 = 1;
pub const SILENT: u32 = 2;
pub const TIMESTAMP_ERROR: u32 = 4;

#[derive(Default)]
pub struct Timeline {
    expected_position: Option<u64>,
    last_qpc: Option<u64>,
}

impl Timeline {
    pub fn stamp(
        &mut self,
        position: u64,
        qpc_100ns: u64,
        frames: u32,
        channels: u8,
        flags: u32,
        now_ns: u64,
    ) -> Result<Stamp, Error> {
        if frames == 0
            || usize::try_from(frames).map_or(true, |n| n > MAX_CHUNK_SAMPLES)
            || !matches!(channels, 1 | 2)
            || flags & !(DISCONTINUITY | SILENT | TIMESTAMP_ERROR) != 0
        {
            return Err(Error::Format);
        }
        if flags & TIMESTAMP_ERROR != 0 || qpc_100ns == 0 {
            return Err(Error::Clock);
        }
        if self
            .expected_position
            .is_some_and(|expected| position < expected)
            || self.last_qpc.is_some_and(|last| qpc_100ns <= last)
        {
            return Err(Error::Discontinuity);
        }
        let captured_ns = qpc_100ns.checked_mul(100).ok_or(Error::Clock)?;
        if captured_ns > now_ns {
            return Err(Error::Clock);
        }
        let valid_until_ns = captured_ns.checked_add(SOURCE_AGE_NS).ok_or(Error::Clock)?;
        // Device and QPC clocks may drift. Keep chronology in the stream's
        // actual sample positions while QPC alone bounds source age; mapping
        // every packet's QPC to 48 kHz would eventually invent gaps/reversals.
        self.expected_position = Some(
            position
                .checked_add(u64::from(frames))
                .ok_or(Error::Clock)?,
        );
        self.last_qpc = Some(qpc_100ns);
        Ok(Stamp {
            timestamp_samples: position,
            valid_until_ns,
            frames,
            channels: u32::from(channels),
        })
    }
}

/// The native owner can hand out a null packet only when SILENT is set. The
/// safe conversion treats those frames as silence without reading native data.
pub fn copy_pcm(
    bytes: Option<&[u8]>,
    values: usize,
    silent: bool,
    output: &mut [i16],
) -> Result<(), Error> {
    if values == 0 || values > output.len() || values > MAX_CHUNK_SAMPLES * 2 {
        return Err(Error::BufferLimit);
    }
    if silent {
        output[..values].fill(0);
        return Ok(());
    }
    let bytes = bytes.ok_or(Error::Format)?;
    if values.checked_mul(2) != Some(bytes.len()) {
        return Err(Error::Format);
    }
    for (value, bytes) in output[..values].iter_mut().zip(bytes.as_chunks::<2>().0) {
        *value = i16::from_le_bytes([bytes[0], bytes[1]]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_audio_capture_policy::Packer;

    #[test]
    fn sustained_qpc_clock_drift_never_invents_a_sample_gap_or_reversal() {
        // Two minutes per direction, +/-200 ppm relative to exact 48 kHz
        // stream positions. Independent QPC flooring fails within seconds.
        for qpc_step in [99_980_u64, 100_020] {
            let mut timeline = Timeline::default();
            let mut packer = Packer::new_exact(1, 960).unwrap();
            for packet in 0..12_000_u64 {
                let qpc = 10_000_000 + packet * qpc_step;
                let now = qpc * 100 + 10_000_000;
                let stamp = timeline.stamp(packet * 480, qpc, 480, 1, 0, now).unwrap();
                assert_eq!(stamp.timestamp_samples, packet * 480);
                assert_eq!(stamp.valid_until_ns, qpc * 100 + SOURCE_AGE_NS);
                packer.push_i16(stamp, &[123; 480], now).unwrap();
                let frames = packer.take(now);
                if packet % 2 == 0 {
                    assert!(frames.is_empty());
                } else {
                    assert_eq!(frames.len(), 1);
                    assert_eq!(frames[0].timestamp_samples, (packet - 1) * 480);
                    assert_eq!(
                        frames[0].valid_until_ns,
                        (qpc - qpc_step) * 100 + SOURCE_AGE_NS
                    );
                }
            }
            assert_eq!(packer.gaps(), 0);
        }
    }

    #[test]
    fn partial_pcm_keeps_original_deadline_through_later_native_reads() {
        let mut timeline = Timeline::default();
        let mut packer = Packer::new_exact(2, 480).unwrap();
        let first = timeline
            .stamp(0, 10_000_000, 240, 2, 0, 1_010_000_000)
            .unwrap();
        packer
            .push_i16(first, &[i16::MIN; 480], 1_010_000_000)
            .unwrap();
        let second = timeline
            .stamp(240, 10_050_000, 240, 2, 0, 1_090_000_000)
            .unwrap();
        packer
            .push_i16(second, &[i16::MAX; 480], 1_090_000_000)
            .unwrap();
        let frames = packer.take(1_099_999_999);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_samples, 0);
        assert_eq!(frames[0].valid_until_ns, 1_100_000_000);
        assert_eq!(&frames[0].samples[..480], &[i16::MIN; 480]);
        assert_eq!(&frames[0].samples[480..], &[i16::MAX; 480]);
        let third = timeline
            .stamp(480, 10_100_000, 480, 2, 0, 1_110_000_000)
            .unwrap();
        packer.push_i16(third, &[100; 960], 1_110_000_000).unwrap();
        assert!(packer.take(1_110_000_000).is_empty());
    }

    #[test]
    fn native_discontinuity_discards_old_samples_without_resetting_the_clock() {
        let mut timeline = Timeline::default();
        let mut packer = Packer::new_exact(1, 480).unwrap();
        let first = timeline
            .stamp(0, 10_000_000, 720, 1, 0, 1_020_000_000)
            .unwrap();
        packer.push_i16(first, &[123; 720], 1_020_000_000).unwrap();
        packer.discontinuity();
        let next = timeline
            .stamp(960, 10_200_000, 480, 1, DISCONTINUITY, 1_030_000_000)
            .unwrap();
        packer.push_i16(next, &[456; 480], 1_030_000_000).unwrap();
        let frames = packer.take(1_030_000_001);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_samples, 960);
        assert_eq!(frames[0].samples.as_ref(), &[456; 480]);
        assert!(packer.gaps() >= 1);
        assert!(matches!(
            timeline.stamp(0, 10_300_000, 480, 1, DISCONTINUITY, 1_040_000_000),
            Err(Error::Discontinuity)
        ));
    }

    #[test]
    fn even_one_missing_stream_sample_discards_the_partial_without_retiming() {
        let mut timeline = Timeline::default();
        let mut packer = Packer::new_exact(1, 480).unwrap();
        let first = timeline
            .stamp(0, 10_000_000, 240, 1, 0, 1_010_000_000)
            .unwrap();
        packer.push_i16(first, &[123; 240], 1_010_000_000).unwrap();
        let next = timeline
            .stamp(241, 10_050_208, 480, 1, 0, 1_020_000_000)
            .unwrap();
        packer.push_i16(next, &[456; 480], 1_020_000_000).unwrap();
        let frames = packer.take(1_020_000_000);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_samples, 241);
        assert_eq!(frames[0].samples.as_ref(), &[456; 480]);
        assert_eq!(packer.gaps(), 2);
    }

    #[test]
    fn unknown_timestamps_flags_shapes_future_and_overflow_are_terminal() {
        for (qpc, frames, channels, flags, now, expected) in [
            (0, 480, 2, 0, 1_000, Error::Clock),
            (10, 480, 2, TIMESTAMP_ERROR, 1_000, Error::Clock),
            (10, 480, 2, 8, 1_000, Error::Format),
            (10, 0, 2, 0, 1_000, Error::Format),
            (10, 4_801, 2, 0, 1_000, Error::Format),
            (10, 480, 3, 0, 1_000, Error::Format),
            (11, 480, 2, 0, 1_000, Error::Clock),
            (u64::MAX, 480, 2, 0, u64::MAX, Error::Clock),
            (u64::MAX / 100, 480, 2, 0, u64::MAX, Error::Clock),
        ] {
            assert!(
                matches!(Timeline::default().stamp(0, qpc, frames, channels, flags, now), Err(error) if error == expected)
            );
        }
        let mut timeline = Timeline::default();
        timeline.stamp(10, 100, 480, 1, 0, 10_000).unwrap();
        assert!(matches!(
            timeline.stamp(490, 99, 480, 1, 0, 10_000),
            Err(Error::Discontinuity)
        ));
        assert!(matches!(
            timeline.stamp(489, 101, 480, 1, 0, 10_100),
            Err(Error::Discontinuity)
        ));
        assert!(matches!(
            Timeline::default().stamp(u64::MAX, 100, 480, 1, 0, 10_000),
            Err(Error::Clock)
        ));
    }

    #[test]
    fn pcm_conversion_preserves_extremes_and_silence_never_reads_a_pointer() {
        let mut out = [999; 4];
        copy_pcm(
            Some(&[0, 128, 255, 127, 255, 255, 0, 0]),
            4,
            false,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, [i16::MIN, i16::MAX, -1, 0]);
        copy_pcm(None, 3, true, &mut out).unwrap();
        assert_eq!(out, [0; 4]);
        assert_eq!(copy_pcm(None, 4, false, &mut out), Err(Error::Format));
        assert_eq!(
            copy_pcm(Some(&[0; 7]), 4, false, &mut out),
            Err(Error::Format)
        );
        assert_eq!(
            copy_pcm(None, usize::MAX, true, &mut out),
            Err(Error::BufferLimit)
        );
        assert_eq!(copy_pcm(None, 0, true, &mut out), Err(Error::BufferLimit));
    }
}
