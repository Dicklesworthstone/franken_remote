#![forbid(unsafe_code)]
//! Exact stream-frame custody at the WASAPI output boundary. Native padding is
//! allocation credit only; the normalized `IAudioClock` is the speaker clock.
use crate::macos_audio_policy::{Error, MAX_QUEUE_SAMPLES};

const RATE: u64 = 48_000;
const TICKS_PER_SECOND: u64 = 10_000_000;
const LEAD_MARGIN: u64 = 240; // 5 ms beyond the endpoint's maximum latency.
const IDLE_MARGIN: u64 = 96; // 2 ms; keep silent startup ahead of consumption.
const MAX_CLOCK_AGE: u64 = 20_000; // 2 ms, in 100 ns units.
const MAX_SLEEP_READ: u64 = 10_000; // 1 ms, including any suspended time.

pub fn samples(position: u64, frequency: u64) -> Result<u64, Error> {
    if frequency == 0 {
        return Err(Error::Clock);
    }
    u64::try_from(u128::from(position) * u128::from(RATE) / u128::from(frequency))
        .map_err(|_| Error::Clock)
}

pub fn fresh_clock(timestamp: u64, now: u64) -> Result<(), Error> {
    if timestamp == 0
        || now
            .checked_sub(timestamp)
            .is_none_or(|age| age > MAX_CLOCK_AGE)
    {
        return Err(Error::Clock);
    }
    Ok(())
}

/// Fixed for this original device and negotiated packet duration. Native
/// latency rounds upward; its lead plus the largest drift-corrected packet
/// must fit the same 40 ms consumed-to-tail ceiling as the other outputs.
pub fn leads(latency_100ns: i64, largest: u32) -> Result<(u64, u64), Error> {
    if !matches!(largest, 481 | 961) {
        return Err(Error::Configuration);
    }
    let ticks = u64::try_from(latency_100ns).map_err(|_| Error::Configuration)?;
    let latency = ticks
        .checked_mul(RATE)
        .ok_or(Error::BufferLimit)?
        .div_ceil(TICKS_PER_SECOND);
    let lead = latency.checked_add(LEAD_MARGIN).ok_or(Error::BufferLimit)?;
    if lead
        .checked_add(u64::from(largest))
        .is_none_or(|n| n > MAX_QUEUE_SAMPLES)
    {
        return Err(Error::BufferLimit);
    }
    Ok((lead, latency + IDLE_MARGIN))
}

/// Startup silence must not occupy the first permitted PCM slot. The native
/// clock starts at zero; its first forward reading schedules PCM after this
/// prefix even on an endpoint whose latency is much shorter than one packet.
pub fn startup_prime(capacity: u32, lead: u64) -> Result<u32, Error> {
    if capacity == 0 || u64::from(capacity) > MAX_QUEUE_SAMPLES || lead == 0 {
        return Err(Error::BufferLimit);
    }
    u32::try_from(u64::from(capacity).min(lead)).map_err(|_| Error::BufferLimit)
}

#[derive(Debug, Clone, Copy)]
pub struct Write {
    pub silence: u32,
    pub frames: u32,
    pub end: u64,
}

/// All writes, including startup silence, have one contiguous stream position.
/// An empty native queue is not repaired by inventing the position of inserted
/// engine silence. That uncertain stream is retired instead.
pub struct Tail {
    written: u64,
}
impl Tail {
    pub fn new(prime: u32) -> Result<Self, Error> {
        if prime == 0 || u64::from(prime) > MAX_QUEUE_SAMPLES {
            return Err(Error::BufferLimit);
        }
        Ok(Self {
            written: u64::from(prime),
        })
    }

    pub fn observe(&self, consumed: u64, padding: u32, capacity: u32) -> Result<(), Error> {
        if capacity == 0 || u64::from(capacity) > MAX_QUEUE_SAMPLES || padding > capacity {
            return Err(Error::BufferLimit);
        }
        let remaining = self.written.checked_sub(consumed).ok_or(Error::Clock)?;
        if padding == 0 || u64::from(padding) > remaining || remaining > MAX_QUEUE_SAMPLES {
            return Err(Error::Clock);
        }
        Ok(())
    }

    pub fn silence(&self, consumed: u64, lead: u64) -> Result<Option<Write>, Error> {
        let end = consumed.checked_add(lead).ok_or(Error::Clock)?;
        if end <= self.written {
            return Ok(None);
        }
        if lead > MAX_QUEUE_SAMPLES {
            return Err(Error::BufferLimit);
        }
        let frames = u32::try_from(end - self.written).map_err(|_| Error::BufferLimit)?;
        Ok(Some(Write {
            silence: frames,
            frames,
            end,
        }))
    }

    pub fn pcm(&self, start: u64, end: u64, frames: u32) -> Result<Write, Error> {
        if frames == 0 || start.checked_add(u64::from(frames)) != Some(end) {
            return Err(Error::Metadata);
        }
        let silence = start.checked_sub(self.written).ok_or(Error::Clock)?;
        let total = silence
            .checked_add(u64::from(frames))
            .ok_or(Error::BufferLimit)?;
        if total > MAX_QUEUE_SAMPLES {
            return Err(Error::BufferLimit);
        }
        Ok(Write {
            silence: u32::try_from(silence).map_err(|_| Error::BufferLimit)?,
            frames: u32::try_from(total).map_err(|_| Error::BufferLimit)?,
            end,
        })
    }

    /// Call only after the exact native `ReleaseBuffer` succeeds. No failed or
    /// abandoned lease advances the count or creates a retry position.
    pub fn committed(&mut self, write: Write) {
        self.written = write.end;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SleepOffset {
    lower: u64,
    upper: u64,
}
impl SleepOffset {
    pub fn new(before: u64, awake: u64, after: u64) -> Result<Self, Error> {
        if after
            .checked_sub(before)
            .is_none_or(|width| width > MAX_SLEEP_READ)
        {
            return Err(Error::Clock);
        }
        Ok(Self {
            // Before the first suspend the offset can be zero, while the
            // middle read is later than before by the sampling interval.
            lower: before.saturating_sub(awake),
            upper: after.checked_sub(awake).ok_or(Error::Clock)?,
        })
    }

    /// Compare every read to the original interval. Intermittent reads never
    /// renew it; one 100 ns rounding unit is the only allowed tolerance.
    pub fn unchanged(self, current: Self) -> Result<(), Error> {
        if current.lower > self.upper.saturating_add(1)
            || self.lower > current.upper.saturating_add(1)
        {
            return Err(Error::Suspended);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_frequency_is_required_and_no_wall_time_creates_samples() {
        assert_eq!(samples(96_001, 96_000), Ok(48_000));
        assert_eq!(samples(10_000_000, 10_000_000), Ok(48_000));
        assert_eq!(samples(480, 0), Err(Error::Clock));
        assert_eq!(samples(u64::MAX, 1), Err(Error::Clock));
        assert_eq!(fresh_clock(10_000, 30_000), Ok(()));
        assert_eq!(fresh_clock(10_000, 30_001), Err(Error::Clock));
        assert_eq!(fresh_clock(10_000, 9_999), Err(Error::Clock));
    }

    #[test]
    fn latency_and_drifted_packet_share_the_native_queue_ceiling() {
        assert_eq!(leads(100_000, 961), Ok((720, 576)));
        assert_eq!(leads(150_000, 961), Err(Error::BufferLimit));
        assert_eq!(leads(150_000, 481), Ok((960, 816)));
        assert_eq!(leads(-1, 481), Err(Error::Configuration));
        assert_eq!(leads(i64::MAX, 481), Err(Error::BufferLimit));
    }

    #[test]
    fn startup_silence_leaves_the_first_low_latency_pcm_slot_available() {
        for (latency, largest) in [(0, 481), (10_000, 961), (100_000, 961)] {
            let (lead, _) = leads(latency, largest).unwrap();
            let prime = startup_prime(1920, lead).unwrap();
            let tail = Tail::new(prime).unwrap();
            tail.observe(1, prime - 1, 1920).unwrap();
            let window = crate::macos_audio_policy::Window {
                slot_start: 1,
                slot_end: 1 + u64::from(largest),
                valid_until: 100_000,
                samples: u64::from(largest),
                lead,
            };
            let (start, end) = window.validate(1, 1).unwrap();
            let first = tail.pcm(start, end, largest).unwrap();
            assert_eq!(first.silence, 1);
            assert_eq!(first.frames, largest + 1);
        }
        assert_eq!(startup_prime(481, 720), Ok(481));
        assert_eq!(startup_prime(0, 240), Err(Error::BufferLimit));
        assert_eq!(startup_prime(1921, 240), Err(Error::BufferLimit));
    }

    #[test]
    fn exact_bounded_silence_precedes_pcm_without_moving_its_slot() {
        let mut tail = Tail::new(960).unwrap();
        tail.observe(480, 240, 1920).unwrap();
        let write = tail.pcm(1200, 1680, 480).unwrap();
        assert_eq!((write.silence, write.frames, write.end), (240, 720, 1680));
        tail.committed(write);
        let next = tail.pcm(1680, 2161, 481).unwrap();
        assert_eq!((next.silence, next.frames), (0, 481));
        assert_eq!(tail.pcm(1679, 2159, 480).err(), Some(Error::Clock));
        assert_eq!(tail.pcm(2000, 2481, 480).err(), Some(Error::Metadata));
    }

    #[test]
    fn failed_or_abandoned_native_lease_keeps_the_original_tail() {
        let mut tail = Tail::new(960).unwrap();
        let first = tail.pcm(1200, 1680, 480).unwrap();
        let retry = tail.pcm(1200, 1680, 480).unwrap();
        assert_eq!(first.silence, retry.silence);
        tail.committed(first);
        assert_eq!(tail.pcm(1200, 1680, 480).err(), Some(Error::Clock));
    }

    #[test]
    fn empty_or_inconsistent_native_queues_never_rebase_the_stream() {
        let tail = Tail::new(960).unwrap();
        assert_eq!(tail.observe(480, 0, 1920), Err(Error::Clock));
        assert_eq!(tail.observe(961, 1, 1920), Err(Error::Clock));
        assert_eq!(tail.observe(480, 481, 1920), Err(Error::Clock));
        assert_eq!(tail.observe(0, 960, 1921), Err(Error::BufferLimit));
        assert!(tail.silence(400, 500).unwrap().is_none());
        let silence = tail.silence(600, 576).unwrap().unwrap();
        assert_eq!(
            (silence.silence, silence.frames, silence.end),
            (216, 216, 1176)
        );
    }

    #[test]
    fn sleep_and_wide_sampling_are_terminal_without_renewing_the_baseline() {
        let zero = SleepOffset::new(1000, 1001, 1002).unwrap();
        zero.unchanged(SleepOffset::new(2000, 2001, 2002).unwrap())
            .unwrap();
        let original = SleepOffset::new(1000, 900, 1002).unwrap();
        original
            .unchanged(SleepOffset::new(2000, 1900, 2002).unwrap())
            .unwrap();
        assert_eq!(
            original.unchanged(SleepOffset::new(20_000, 1900, 20_002).unwrap()),
            Err(Error::Suspended)
        );
        assert_eq!(
            SleepOffset::new(1000, 900, 11_001).err(),
            Some(Error::Clock)
        );
        assert_eq!(SleepOffset::new(1000, 1100, 1002).err(), Some(Error::Clock));
    }
}
