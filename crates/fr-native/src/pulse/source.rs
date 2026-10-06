#![forbid(unsafe_code)]
//! One audio generation of host playback capture: the selected monitor's
//! RECORD stream, one bounded PCM frame accumulator and the real libopus
//! encoder. Each `pull` drains what the server already recorded and returns
//! at most `MAX_BATCH_PACKETS` encoded packets; it never blocks or sleeps.
//!
//! The source timeline counts delivered samples plus explicit server holes.
//! A hole drops the partial frame (counted as a gap) and the next packet's
//! timestamp jumps, so a discontinuity is never hidden or filled with
//! invented sound. A pull drains the bounded native queue before encoding,
//! retaining only its newest complete PCM frames. Superseded raw frames are
//! counted as gaps; superseded work never spends Opus encoding time. A pull
//! that finds the bounded server queue full is counted as a possible overrun:
//! the server may already have discarded older audio there.
use super::{
    Error,
    capture::{CaptureDevice, CaptureSelection, Chunk},
};
use crate::opus::{CodecLimits, Encoder};
use fr_core::audio::{AudioDirection, AudioStreamConfig, DEFAULT_JITTER_TARGET_MS};
use fr_media::{
    audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame},
    worker::audio::{Capture, CaptureCounters, MAX_BATCH_PACKETS},
};
use std::path::Path;

/// Stereo 20 ms at 48 kHz.
const MAX_FRAME_VALUES: usize = 2 * 960;
/// Complete raw frames retained per pull, independently of the read budget.
/// Same maximum encoder work as the former normal four-frame pull.
const PULL_FRAMES: usize = 4;
const _: () = assert!(PULL_FRAMES <= MAX_BATCH_PACKETS);

mod recent;
use recent::Recent;

pub struct PlaybackSource {
    device: CaptureDevice,
    encoder: Encoder,
    capture: Capture,
    frame: Box<[i16; MAX_FRAME_VALUES]>,
    /// Fixed scratch, emptied on every pull; never a cross-pull PCM FIFO.
    recent: Recent,
    filled: usize,
    carry: Option<u8>,
    /// Source-timeline sample index of `frame[0]`.
    start: u64,
    counters: CaptureCounters,
}
impl std::fmt::Debug for PlaybackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackSource")
            .field("capture", &self.capture)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}
impl PlaybackSource {
    /// Validate the whole profile and create the real encoder BEFORE the
    /// monitor connection; `poll_ready` completes the native stream setup.
    pub fn connect(capture: Capture, now_us: u64) -> Result<Self, Error> {
        capture.validate().map_err(|_| Error::Configuration)?;
        let config = AudioStreamConfig::new(
            AudioDirection::Downlink,
            capture.generation,
            capture.channels,
            capture.frame_duration_ms,
            DEFAULT_JITTER_TARGET_MS,
        )
        .map_err(|_| Error::Configuration)?;
        let limits = CodecLimits::new(
            usize::from(capture.max_packet_bytes),
            u32::from(capture.frame_samples()),
        )
        .map_err(|_| Error::Configuration)?;
        let mut encoder =
            Encoder::with_limits(limits, capture.bitrate, 5).map_err(|_| Error::Configuration)?;
        encoder
            .configure(config)
            .map_err(|_| Error::Configuration)?;
        let selection = CaptureSelection::new(Path::new(&capture.server), &capture.monitor)?;
        let device = CaptureDevice::connect(selection, capture.channels, now_us)?;
        Ok(Self {
            device,
            encoder,
            capture,
            frame: Box::new([0; MAX_FRAME_VALUES]),
            recent: Recent::new(),
            filled: 0,
            carry: None,
            start: 0,
            counters: CaptureCounters::default(),
        })
    }
    /// Bounded native progress; true once the monitor stream records.
    pub fn poll_ready(&mut self, now_us: u64) -> Result<bool, Error> {
        self.device.poll(now_us)
    }
    pub const fn capture(&self) -> &Capture {
        &self.capture
    }
    fn channels(&self) -> usize {
        usize::from(self.capture.channels.count())
    }
    fn frame_values(&self) -> usize {
        usize::from(self.capture.frame_samples()) * self.channels()
    }
    /// Drain the bounded queue, then encode only its newest complete frames.
    /// No codec call runs while a `PulseAudio` fragment is borrowed. Error or
    /// normal return empties the scratch; failure permanently retires the source.
    pub fn pull(&mut self, now_us: u64) -> Result<(Vec<AudioAccessUnit>, CaptureCounters), Error> {
        self.recent.clear();
        let result = self.pull_inner(now_us);
        self.recent.clear();
        if result.is_err() {
            self.disconnect();
        }
        result
    }
    fn pull_inner(&mut self, now_us: u64) -> Result<(Vec<AudioAccessUnit>, CaptureCounters), Error> {
        let values = self.frame_values();
        let channels = self.channels();
        // Do not leave newer server audio behind merely because four OLD
        // frames were read. CaptureDevice validates this negotiated byte bound.
        let budget = usize::try_from(self.device.queue_bytes()).map_err(|_| Error::BufferLimit)?;
        let Self {
            device,
            encoder,
            capture,
            frame,
            recent,
            filled,
            carry,
            start,
            counters,
        } = self;
        let mut state = Accumulator {
            frame,
            filled,
            carry,
            start,
            values,
            channels,
        };
        let (_, full) = device.read(now_us, budget, |chunk| {
            let bytes = match chunk {
                Chunk::Bytes(bytes) => bytes.len(),
                Chunk::Hole(bytes) => bytes,
            };
            // Bound work even if a native fragment violates the queue contract.
            // The read loop itself stops after budget bytes plus one fragment.
            if bytes > budget {
                return Err(Error::BufferLimit);
            }
            if let Chunk::Hole(_) = chunk {
                counters.gaps = counters.gaps.saturating_add(1);
            }
            state.push(chunk, &mut |pcm, timestamp| recent.push(pcm, timestamp))
        })?;
        counters.gaps = counters.gaps.saturating_add(recent.dropped());
        if full {
            counters.overruns = counters.overruns.saturating_add(1);
        }
        let mut packets = Vec::with_capacity(PULL_FRAMES);
        for (pcm, timestamp) in recent.iter() {
            packets.push(encode(encoder, capture, pcm, timestamp)?);
        }
        Ok((packets, *counters))
    }
    pub fn disconnect(&mut self) {
        self.device.disconnect();
        self.encoder.close();
        self.recent.clear();
        self.frame.fill(0);
        self.filled = 0;
        self.carry = None;
    }
}
fn encode(
    encoder: &mut Encoder,
    capture: &Capture,
    pcm: &[i16],
    timestamp: u64,
) -> Result<AudioAccessUnit, Error> {
    let frame =
        AudioPcmFrame::from_interleaved(capture.generation, capture.channels, timestamp, pcm)
            .map_err(|_| Error::Metadata)?;
    encoder.submit_pcm(&frame).map_err(|_| Error::Native)?;
    encoder
        .poll_packet()
        .map_err(|_| Error::Native)?
        .ok_or(Error::Native)
}
struct Accumulator<'a> {
    frame: &'a mut [i16; MAX_FRAME_VALUES],
    filled: &'a mut usize,
    carry: &'a mut Option<u8>,
    start: &'a mut u64,
    values: usize,
    channels: usize,
}
impl Accumulator<'_> {
    fn push(
        &mut self,
        chunk: Chunk<'_>,
        emit: &mut impl FnMut(&[i16], u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let bytes = match chunk {
            Chunk::Hole(bytes) => {
                // Explicit discontinuity: drop the partial frame, advance time.
                let stride = self.channels * 2;
                let skipped = u64::try_from(*self.filled / self.channels + bytes / stride)
                    .map_err(|_| Error::Clock)?;
                *self.start = self.start.checked_add(skipped).ok_or(Error::Clock)?;
                *self.filled = 0;
                *self.carry = None;
                return Ok(());
            }
            Chunk::Bytes(bytes) => bytes,
        };
        let mut rest = bytes;
        // Complete one sample split across two server fragments.
        if let Some(low) = self.carry.take() {
            let Some((&high, tail)) = rest.split_first() else {
                *self.carry = Some(low);
                return Ok(());
            };
            rest = tail;
            self.value(i16::from_ne_bytes([low, high]), emit)?;
        }
        let (samples, tail) = rest.as_chunks::<2>();
        for pair in samples {
            self.value(i16::from_ne_bytes(*pair), emit)?;
        }
        *self.carry = tail.first().copied();
        Ok(())
    }
    fn value(
        &mut self,
        value: i16,
        emit: &mut impl FnMut(&[i16], u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.frame[*self.filled] = value;
        *self.filled += 1;
        if *self.filled == self.values {
            emit(&self.frame[..self.values], *self.start)?;
            *self.filled = 0;
            let per_channel =
                u64::try_from(self.values / self.channels).map_err(|_| Error::Clock)?;
            *self.start = self.start.checked_add(per_channel).ok_or(Error::Clock)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
