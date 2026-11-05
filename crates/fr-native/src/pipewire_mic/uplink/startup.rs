//! Bounded real PCM preparation before the first native sample is scheduled.
//!
//! A codec frame may be smaller than one graph quantum. Keep the original
//! decoded frames until they cover that first quantum, then consume them in one
//! bounded admission turn. This owner neither invents PLC nor assigns a native
//! sample position while the real decoder is still preparing the batch.
use fr_client::audio::playout::drift::Span;
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::time::Instant;

// The native boundary supports graph quanta through 1024 samples. Three 480
// sample frames or two 960 sample frames cover every supported first quantum.
const MAX_FRAMES: usize = 3;
const MAX_QUANTUM: u32 = 1024;
const MAX_SAMPLES: u32 = 1920; // Per channel, including the final carried tail.
const MAX_BYTES: usize = MAX_SAMPLES as usize * 2 * size_of::<i16>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Configuration,
    Metadata,
    Capacity,
    Expired,
    Timeline,
}

pub(super) struct Pcm(pub(super) AudioPcmFrame);
impl Drop for Pcm {
    fn drop(&mut self) {
        self.0.samples_mut().fill(0);
    }
}

pub(super) struct Frame {
    pub(super) pcm: Pcm,
    pub(super) span: Span,
    pub(super) expires: Instant,
}

pub(super) struct Startup {
    config: AudioStreamConfig,
    frames: Vec<Frame>,
    samples: u32,
    bytes: usize,
    quantum: Option<u32>,
    source_end: Option<u64>,
    sealed: bool,
}
impl Startup {
    pub(super) fn new(config: AudioStreamConfig) -> Result<Self, Error> {
        if config.direction() != AudioDirection::Uplink
            || config.sample_rate() != 48_000
            || !matches!(config.expected_samples_per_frame(), 480 | 960)
        {
            return Err(Error::Configuration);
        }
        // AudioPcmFrame has fixed backing for the maximum codec duration, even
        // for a 10-ms frame. Reserve its full bounded storage once; never grow
        // this vector while serving a graph or put three such frames on stack.
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(MAX_FRAMES)
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            config,
            frames,
            samples: 0,
            bytes: 0,
            quantum: None,
            source_end: None,
            sealed: false,
        })
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn check_quantum(&self, quantum: u32) -> Result<(), Error> {
        if self.sealed {
            return Err(Error::Metadata);
        }
        if quantum == 0 || quantum > MAX_QUANTUM {
            return Err(Error::Configuration);
        }
        if self.quantum.is_some_and(|original| original != quantum) {
            return Err(Error::Metadata);
        }
        Ok(())
    }

    pub(super) fn needs_frame(&self, quantum: u32) -> Result<bool, Error> {
        self.check_quantum(quantum)?;
        Ok(self.samples < quantum)
    }

    pub(super) fn check_live(&mut self, now: Instant) -> Result<(), Error> {
        if self.frames.iter().any(|frame| now >= frame.expires) {
            self.clear();
            return Err(Error::Expired);
        }
        Ok(())
    }

    pub(super) fn push(&mut self, frame: Frame, quantum: u32, now: Instant) -> Result<(), Error> {
        let result = self.push_inner(frame, quantum, now);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn push_inner(&mut self, frame: Frame, quantum: u32, now: Instant) -> Result<(), Error> {
        self.check_live(now)?;
        if now >= frame.expires {
            return Err(Error::Expired);
        }
        if !self.needs_frame(quantum)? || self.frames.len() == MAX_FRAMES {
            return Err(Error::Capacity);
        }
        let pcm = &frame.pcm.0;
        let nominal = self.config.expected_samples_per_frame();
        if pcm.generation() != self.config.generation()
            || pcm.channels() != self.config.channels()
            || pcm.sample_rate() != self.config.sample_rate()
            || pcm.samples_per_channel() != usize::try_from(nominal).map_err(|_| Error::Metadata)?
            || frame.span.samples() != nominal
            || self
                .source_end
                .is_some_and(|end| pcm.timestamp_samples() != end)
        {
            return Err(Error::Metadata);
        }
        let source_end = pcm
            .timestamp_samples()
            .checked_add(u64::from(nominal))
            .ok_or(Error::Timeline)?;
        let samples = self.samples.checked_add(nominal).ok_or(Error::Capacity)?;
        let bytes = pcm
            .total_samples()
            .checked_mul(size_of::<i16>())
            .and_then(|bytes| self.bytes.checked_add(bytes))
            .ok_or(Error::Capacity)?;
        if samples > MAX_SAMPLES || bytes > MAX_BYTES {
            return Err(Error::Capacity);
        }
        self.frames.push(frame);
        self.samples = samples;
        self.bytes = bytes;
        self.quantum = Some(quantum);
        self.source_end = Some(source_end);
        Ok(())
    }

    /// An incomplete batch or temporary native capacity cannot consume PCM or
    /// assign a sample position. A complete batch contains the smallest prefix
    /// covering the quantum, so every frame starts inside that same interval.
    pub(super) fn take(
        &mut self,
        quantum: u32,
        available_blocks: usize,
        now: Instant,
    ) -> Result<Option<Batch>, Error> {
        self.check_live(now)?;
        if self.needs_frame(quantum)? || self.frames.len() > available_blocks {
            return Ok(None);
        }
        let batch = Batch {
            frames: std::mem::take(&mut self.frames),
            samples: self.samples,
        };
        self.clear();
        Ok(Some(batch))
    }

    pub(super) fn clear(&mut self) {
        self.frames.clear();
        self.samples = 0;
        self.bytes = 0;
        self.quantum = None;
        self.source_end = None;
        self.sealed = true;
    }
}

pub(super) struct Batch {
    frames: Vec<Frame>,
    samples: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Submitted {
    pub(super) source_samples: u64,
    pub(super) output_samples: u32,
}
impl Batch {
    /// The caller supplies one fresh native demand's start, and must not poll
    /// the native loop between these admissions. Each frame is borrowed and
    /// erased in place as it is copied; an error drops the remainder and the containing
    /// native operation must fence/clear any partial admission before polling.
    pub(super) fn admit<E: From<Error>>(
        mut self,
        mut start: u64,
        mut now: impl FnMut() -> Instant,
        mut publish: impl FnMut(&mut Frame, u64) -> Result<(), E>,
    ) -> Result<Submitted, E> {
        let source_samples = self
            .frames
            .first()
            .ok_or(Error::Metadata)?
            .pcm
            .0
            .timestamp_samples();
        start
            .checked_add(u64::from(self.samples))
            .ok_or(Error::Timeline)?;
        let expires = self
            .frames
            .iter()
            .map(|frame| frame.expires)
            .min()
            .ok_or(Error::Metadata)?;
        for frame in &mut self.frames {
            if now() >= expires {
                return Err(Error::Expired.into());
            }
            let samples = frame.span.samples();
            let result = publish(frame, start);
            frame.pcm.0.samples_mut().fill(0);
            result?;
            if now() >= expires {
                return Err(Error::Expired.into());
            }
            start += u64::from(samples); // Entire batch checked before admission.
        }
        Ok(Submitted {
            source_samples,
            output_samples: self.samples,
        })
    }
}

#[cfg(test)]
mod tests;
