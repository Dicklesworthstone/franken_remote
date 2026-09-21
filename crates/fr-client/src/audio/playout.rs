#![forbid(unsafe_code)]
//! Clock-paced, single-owner audio decode and immediate bounded submission.
//!
//! Supply an already admitted/acknowledged direction, a real worker-confined
//! decoder, and the output device's 48 kHz sample clock. The local checkpoint
//! must recheck that direction's current observation/talk permission and return
//! a fresh clock reading. It runs before decode AND immediately before output.
//! Submission must be nonblocking, preserve the supplied epoch/deadline and
//! enforce permission and BOTH clock bounds at the actual OS boundary; success means submitted, not
//! audibly observed. No input authority, device, thread or network is created.
//!
//! Use outside the realtime callback and authority thread: a decoder can enter
//! native code. The containing supervised worker remains responsible for hung
//! foreign calls and for clearing the device's own bounded queue on retirement.

use super::jitter::{JITTER_BUFFER_CAPACITY, JitterError};
use super::{AudioJitterBuffer, AudioVolumeControl, JitterDrainResult};
use crate::input::ClientInstant;
use fr_core::audio::{
    AudioDirection, AudioGeneration, AudioStreamConfig, MAX_JITTER_CEILING_MS, OPUS_SAMPLE_RATE,
};
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioMediaError, AudioPcmFrame};

const MAX_AGE_US: u64 = MAX_JITTER_CEILING_MS as u64 * 1000;

/// Two independent LOCAL readings. `output_samples` is the device's consumed
/// sample position, in 48 kHz per-channel units, not packet count or host time.
/// Device discontinuity/suspend requires retirement, never counter relabeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutClock {
    pub now: ClientInstant,
    pub output_samples: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayoutError {
    Stopped,
    Denied,
    ClockRegression,
    ClockOverflow,
    Expired,
    MissedDeviceSlot,
    Configuration,
    GenerationNotAdvanced,
    Jitter(JitterError),
    Codec(AudioMediaError),
    CodecOutput,
    Output,
}
impl core::fmt::Display for PlayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "audio-playout: {self:?}")
    }
}
impl std::error::Error for PlayoutError {}

/// Metadata only. This is not a receipt for observed sound or captured activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSubmission {
    pub direction: AudioDirection,
    pub generation: AudioGeneration,
    pub sequence: u64,
    pub source_samples: u64,
    pub output_samples: u64,
    pub valid_until: ClientInstant,
    pub output_valid_before: u64,
    pub concealed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayoutResult {
    Waiting,
    Submitted(AudioSubmission),
}

#[derive(Clone, Copy)]
struct Arrival {
    sequence: u64,
    until: u64,
}

/// One configured direction, one decoder and the existing 16-packet jitter
/// bound. At most one decoded PCM frame is alive across the submission callback;
/// there is no decoded FIFO or public mutable decoder escape.
pub struct AudioPlayout<D: AudioDecoder> {
    decoder: Option<D>,
    config: AudioStreamConfig,
    floor: AudioGeneration,
    jitter: AudioJitterBuffer,
    arrivals: [Option<Arrival>; JITTER_BUFFER_CAPACITY],
    volume: AudioVolumeControl,
    last: PlayoutClock,
    next_output: Option<u64>,
    progress_until: Option<u64>,
    error: Option<PlayoutError>,
}
impl<D: AudioDecoder> AudioPlayout<D> {
    pub fn new(
        config: AudioStreamConfig,
        mut decoder: D,
        clock: PlayoutClock,
    ) -> Result<Self, PlayoutError> {
        let jitter = Self::configured_jitter(config)?;
        decoder.configure(config).map_err(PlayoutError::Codec)?;
        Ok(Self {
            decoder: Some(decoder),
            config,
            floor: config.generation(),
            jitter,
            arrivals: [None; JITTER_BUFFER_CAPACITY],
            volume: AudioVolumeControl::new(),
            last: clock,
            next_output: None,
            progress_until: None,
            error: None,
        })
    }
    fn configured_jitter(config: AudioStreamConfig) -> Result<AudioJitterBuffer, PlayoutError> {
        // A target at the expiry boundary cannot admit a timely first output.
        if config.jitter_target_ms() >= MAX_JITTER_CEILING_MS {
            return Err(PlayoutError::Configuration);
        }
        AudioJitterBuffer::with_config(config).map_err(PlayoutError::Jitter)
    }
    pub const fn error(&self) -> Option<PlayoutError> {
        self.error
    }
    pub const fn generation(&self) -> AudioGeneration {
        self.floor
    }
    pub const fn queued_packets(&self) -> usize {
        self.jitter.queued_packet_count()
    }
    pub fn volume_mut(&mut self) -> &mut AudioVolumeControl {
        &mut self.volume
    }
    pub fn stop(&mut self) {
        self.retire(PlayoutError::Stopped);
    }
    fn retire(&mut self, error: PlayoutError) {
        // Fence before native destruction, including unwind/error paths.
        if self.error.is_none() {
            self.error = Some(error);
        }
        self.jitter.stop();
        self.arrivals.fill(None);
        self.next_output = None;
        self.progress_until = None;
        self.decoder = None;
    }

    /// Replace a retired/changed device with a NEW decoder and strictly newer
    /// generation. Failure retires both the old stream and the attempted epoch;
    /// no old sound or buffered native output can survive a failed reconfigure.
    /// Direction stays fixed: downlink permission can never become uplink consent.
    pub fn reconfigure(
        &mut self,
        config: AudioStreamConfig,
        mut decoder: D,
        clock: PlayoutClock,
    ) -> Result<(), PlayoutError> {
        self.stop();
        if !config.generation().supersedes(self.floor) {
            return Err(PlayoutError::GenerationNotAdvanced);
        }
        self.floor = config.generation();
        if config.direction() != self.config.direction() {
            return Err(PlayoutError::Configuration);
        }
        let jitter = Self::configured_jitter(config)?;
        decoder.configure(config).map_err(PlayoutError::Codec)?;
        self.decoder = Some(decoder);
        self.config = config;
        self.jitter = jitter;
        self.last = clock;
        self.error = None;
        Ok(())
    }

    /// Call only on the original enabled/acknowledged audio channel. Arrival
    /// deadlines are fixed on first admission; retries/duplicates cannot refresh
    /// age or move the output schedule. This clock is never host source freshness.
    pub fn receive(
        &mut self,
        packet: AudioAccessUnit,
        clock: PlayoutClock,
    ) -> Result<bool, PlayoutError> {
        self.check(clock)?;
        let until = clock
            .now
            .0
            .checked_add(MAX_AGE_US)
            .ok_or(PlayoutError::ClockOverflow);
        let until = match until {
            Ok(until) => until,
            Err(error) => {
                self.retire(error);
                return Err(error);
            }
        };
        match self.jitter.try_push_packet(packet) {
            Ok(false) => return Ok(false),
            Err(error) => {
                let terminal = self.jitter.error().is_some();
                let error = PlayoutError::Jitter(error);
                if terminal {
                    self.retire(error);
                }
                return Err(error);
            }
            Ok(true) => {}
        }
        // Startup eviction belongs to the jitter owner. Its bounded metadata
        // mirror retains exactly the packets still queued, never an unbounded map.
        for arrival in &mut self.arrivals {
            if arrival.is_some_and(|a| !self.jitter.contains_sequence(a.sequence)) {
                *arrival = None;
            }
        }
        let Some(slot) = self.arrivals.iter_mut().find(|a| a.is_none()) else {
            self.retire(PlayoutError::CodecOutput);
            return Err(PlayoutError::CodecOutput);
        };
        *slot = Some(Arrival {
            sequence: packet.sequence(),
            until,
        });
        if self.next_output.is_none() {
            let target =
                u64::from(self.config.jitter_target_ms()) * u64::from(OPUS_SAMPLE_RATE / 1000);
            let Some(next) = clock.output_samples.checked_add(target) else {
                self.retire(PlayoutError::ClockOverflow);
                return Err(PlayoutError::ClockOverflow);
            };
            self.next_output = Some(next);
            self.progress_until = Some(until);
        }
        Ok(true)
    }
    fn check(&mut self, clock: PlayoutClock) -> Result<(), PlayoutError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let error =
            if clock.now.0 < self.last.now.0 || clock.output_samples < self.last.output_samples {
                Some(PlayoutError::ClockRegression)
            } else if self
                .progress_until
                .is_some_and(|until| clock.now.0 >= until)
                || self
                    .arrivals
                    .iter()
                    .flatten()
                    .any(|a| clock.now.0 >= a.until)
            {
                Some(PlayoutError::Expired)
            } else {
                None
            };
        if let Some(error) = error {
            self.retire(error);
            return Err(error);
        }
        self.last = clock;
        Ok(())
    }

    /// At most one packet/PLC and one output submission. Repeated calls at the
    /// same device position do nothing. A missed whole frame retires instead of
    /// bursting through stale sound or silently changing the codec timeline.
    ///
    /// `checkpoint` must check live direction/epoch permission, including local
    /// revoke/device events, and return a fresh local clock or Denied. `submit`
    /// must enforce that same permission/deadline at actual OS submission, under
    /// its local authority owner. It must not retain this borrowed PCM, block or
    /// create an unbounded device queue. Both callbacks run without library locks.
    pub fn render(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<PlayoutClock, PlayoutError>,
        submit: impl FnOnce(&AudioPcmFrame, AudioSubmission) -> Result<(), PlayoutError>,
    ) -> Result<PlayoutResult, PlayoutError> {
        let mut operation = Operation {
            owner: self,
            completed: false,
        };
        let result = operation.owner.render_inner(&mut checkpoint, submit);
        if let Err(error) = result {
            operation.owner.retire(error);
        }
        operation.completed = true;
        result
    }
    fn valid_pcm(&self, pcm: &AudioPcmFrame, at: u64) -> bool {
        pcm.generation() == self.config.generation()
            && pcm.channels() == self.config.channels()
            && pcm.sample_rate() == OPUS_SAMPLE_RATE
            && u32::try_from(pcm.samples_per_channel()).ok()
                == Some(self.config.expected_samples_per_frame())
            && pcm.timestamp_samples() == at
    }
    fn render_inner(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<PlayoutClock, PlayoutError>,
        submit: impl FnOnce(&AudioPcmFrame, AudioSubmission) -> Result<(), PlayoutError>,
    ) -> Result<PlayoutResult, PlayoutError> {
        let clock = checkpoint()?;
        self.check(clock)?;
        let Some(due) = self.next_output else {
            return Ok(PlayoutResult::Waiting);
        };
        if clock.output_samples < due {
            return Ok(PlayoutResult::Waiting);
        }
        let next = due
            .checked_add(u64::from(self.config.expected_samples_per_frame()))
            .ok_or(PlayoutError::ClockOverflow)?;
        if clock.output_samples >= next {
            return Err(PlayoutError::MissedDeviceSlot);
        }
        let missing_at = self.jitter.next_sample_timestamp();
        let decoder = self.decoder.as_mut().ok_or(PlayoutError::Stopped)?;
        let (mut pcm, sequence, at, concealed, until) = match self
            .jitter
            .drain_for_playout()
            .map_err(PlayoutError::Jitter)?
        {
            JitterDrainResult::Underrun => return Ok(PlayoutResult::Waiting),
            JitterDrainResult::Packet(packet) => {
                let entry = self
                    .arrivals
                    .iter_mut()
                    .find(|a| a.is_some_and(|a| a.sequence == packet.sequence()))
                    .ok_or(PlayoutError::CodecOutput)?;
                let until = entry.take().ok_or(PlayoutError::CodecOutput)?.until;
                decoder
                    .submit_packet(&packet)
                    .map_err(PlayoutError::Codec)?;
                let pcm = decoder
                    .poll_pcm()
                    .map_err(PlayoutError::Codec)?
                    .ok_or(PlayoutError::CodecOutput)?;
                (
                    Scrub(pcm),
                    packet.sequence(),
                    packet.timestamp_samples(),
                    false,
                    until,
                )
            }
            JitterDrainResult::Plc {
                missing_sequence,
                duration_samples,
            } => {
                let at = missing_at.ok_or(PlayoutError::CodecOutput)?;
                let pcm = decoder
                    .decode_plc(duration_samples)
                    .map_err(PlayoutError::Codec)?;
                (
                    Scrub(pcm),
                    missing_sequence,
                    at,
                    true,
                    self.progress_until.ok_or(PlayoutError::CodecOutput)?,
                )
            }
        };
        if !self.valid_pcm(&pcm.0, at) {
            return Err(PlayoutError::CodecOutput);
        }
        // Native decode may be slow, or local permission may have changed while
        // it ran. Never turn an earlier admission into a refreshed output grant.
        let fresh = checkpoint()?;
        self.check(fresh)?;
        if fresh.now.0 >= until {
            return Err(PlayoutError::Expired);
        }
        if fresh.output_samples >= next {
            return Err(PlayoutError::MissedDeviceSlot);
        }
        let progress = fresh
            .now
            .0
            .checked_add(u64::from(self.config.frame_duration_ms()) * 1000)
            .and_then(|end| end.checked_add(MAX_AGE_US))
            .ok_or(PlayoutError::ClockOverflow)?;
        self.volume.apply_to_pcm(&mut pcm.0);
        let receipt = AudioSubmission {
            direction: self.config.direction(),
            generation: self.config.generation(),
            sequence,
            source_samples: at,
            output_samples: due,
            valid_until: ClientInstant(until),
            output_valid_before: next,
            concealed,
        };
        submit(&pcm.0, receipt)?;
        self.next_output = Some(next);
        if !concealed {
            self.progress_until = Some(progress);
        }
        Ok(PlayoutResult::Submitted(receipt))
    }
}
impl<D: AudioDecoder> Drop for AudioPlayout<D> {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Operation<'a, D: AudioDecoder> {
    owner: &'a mut AudioPlayout<D>,
    completed: bool,
}
impl<D: AudioDecoder> Drop for Operation<'_, D> {
    fn drop(&mut self) {
        if !self.completed {
            self.owner.retire(PlayoutError::Stopped);
        }
    }
}
struct Scrub(AudioPcmFrame);
impl Drop for Scrub {
    fn drop(&mut self) {
        self.0.samples_mut().fill(0);
    }
}
