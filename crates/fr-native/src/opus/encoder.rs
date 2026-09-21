use super::{check_generation, ffi, frame_samples, state_bytes};
use fr_core::audio::{AudioGeneration, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES};
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioMediaError, AudioPcmFrame};
use std::{ffi::c_void, fmt, ptr::NonNull};

struct State {
    // NonNull is deliberately !Send and !Sync: native state remains worker-local.
    ptr: NonNull<c_void>,
    bytes: usize,
    lookahead: u16,
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: the pointer comes from create, is uniquely owned, and no call
        // retains a borrow. Exactly one matching native destroy owns deallocation.
        unsafe { ffi::opus_encoder_destroy(self.ptr.as_ptr()) };
    }
}
impl State {
    fn new(
        config: AudioStreamConfig,
        bitrate: i32,
        complexity: i32,
    ) -> Result<Self, AudioMediaError> {
        let channels = i32::from(config.channels().count());
        // SAFETY: channels is a validated mono/stereo enum; this query has no state.
        let bytes = state_bytes(unsafe { ffi::opus_encoder_get_size(channels) })?;
        let mut error = 0;
        // SAFETY: rate/channels/application are admitted, error is writable. No
        // input is retained. Check both pointer and status before any use.
        let ptr = unsafe {
            ffi::opus_encoder_create(48_000, channels, ffi::APPLICATION_AUDIO, &raw mut error)
        };
        let mut state = Self {
            ptr: NonNull::new(ptr).ok_or(AudioMediaError::Fatal)?,
            bytes,
            lookahead: 0,
        };
        if error != 0 {
            return Err(AudioMediaError::Fatal);
        }
        let mut lookahead = 0_i32;
        // SAFETY: CTL requests take the exact promoted C int/int-pointer argument
        // declared by opus_defines.h. This owner is not shared with another call.
        let (rate_result, complexity_result, delay_result) = unsafe {
            (
                ffi::opus_encoder_ctl(state.ptr.as_ptr(), ffi::SET_BITRATE, bitrate),
                ffi::opus_encoder_ctl(state.ptr.as_ptr(), ffi::SET_COMPLEXITY, complexity),
                ffi::opus_encoder_ctl(state.ptr.as_ptr(), ffi::GET_LOOKAHEAD, &raw mut lookahead),
            )
        };
        if rate_result != 0 || complexity_result != 0 || delay_result != 0 {
            return Err(AudioMediaError::Fatal);
        }
        let lookahead = u16::try_from(lookahead).map_err(|_| AudioMediaError::Fatal)?;
        state.lookahead = lookahead;
        Ok(state)
    }
}

/// Real Opus AUDIO encoder: 48 kHz mono/stereo, one pending packet, no input FIFO.
///
/// Call only on the worker's codec thread, off input-authority and realtime audio
/// callback paths. Reconfiguration requires a strictly newer audio generation;
/// polling output never fabricates or renews capture authority.
pub struct Encoder {
    state: Option<State>,
    config: Option<AudioStreamConfig>,
    generation: Option<AudioGeneration>,
    bitrate: i32,
    complexity: i32,
    next_sequence: u64,
    last_end: Option<u64>,
    pending: Option<AudioAccessUnit>,
}
impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusEncoder")
            .field("configuration", &self.config)
            .field("native_bytes", &self.native_state_bytes())
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}
impl Encoder {
    /// Creates an unconfigured owner. Allocation occurs only after configure.
    pub const fn new() -> Self {
        Self {
            state: None,
            config: None,
            generation: None,
            bitrate: 96_000,
            complexity: 5,
            next_sequence: 0,
            last_end: None,
            pending: None,
        }
    }

    /// Bounded local quality choice; this is not a peer-controlled native option.
    pub fn with_settings(bitrate: u32, complexity: u8) -> Result<Self, AudioMediaError> {
        if !(6_000..=192_000).contains(&bitrate) || complexity > 10 {
            return Err(AudioMediaError::UnsupportedFormat);
        }
        Ok(Self {
            bitrate: i32::try_from(bitrate).map_err(|_| AudioMediaError::UnsupportedFormat)?,
            complexity: i32::from(complexity),
            ..Self::new()
        })
    }

    pub fn native_state_bytes(&self) -> usize {
        self.state.as_ref().map_or(0, |state| state.bytes)
    }

    /// Codec delay only, in source samples; not a playout or network measurement.
    pub fn lookahead_samples(&self) -> Result<u16, AudioMediaError> {
        self.state
            .as_ref()
            .map(|state| state.lookahead)
            .ok_or(AudioMediaError::NotConfigured)
    }

    /// Discard obsolete output and release native state. Only a newer generation
    /// may configure again, including after a fatal native call.
    pub fn close(&mut self) {
        self.pending = None;
        self.state = None;
        self.config = None;
        self.last_end = None;
    }
}
impl AudioEncoder for Encoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        if self.pending.is_some() {
            return Err(AudioMediaError::Backpressure);
        }
        frame_samples(config)?;
        check_generation(self.generation, config.generation())?;
        // At most two capped states during a successful reconfiguration. Failure
        // drops the candidate and preserves the previous usable stream.
        let state = State::new(config, self.bitrate, self.complexity)?;
        self.state = Some(state);
        self.config = Some(config);
        self.generation = Some(config.generation());
        self.next_sequence = 0;
        self.last_end = None;
        Ok(())
    }

    fn submit_pcm(&mut self, pcm: &AudioPcmFrame) -> Result<(), AudioMediaError> {
        let config = self.config.ok_or(AudioMediaError::NotConfigured)?;
        if pcm.generation() != config.generation() {
            return Err(AudioMediaError::GenerationMismatch {
                expected: config.generation(),
                found: pcm.generation(),
            });
        }
        if self.pending.is_some() {
            return Err(AudioMediaError::Backpressure);
        }
        let samples = frame_samples(config)?;
        if pcm.channels() != config.channels()
            || pcm.sample_rate() != config.sample_rate()
            || pcm.samples_per_channel() != usize::from(samples)
            || self
                .last_end
                .is_some_and(|end| pcm.timestamp_samples() < end)
        {
            return Err(AudioMediaError::InvalidPayload);
        }
        let end = pcm
            .timestamp_samples()
            .checked_add(u64::from(samples))
            .ok_or(AudioMediaError::BufferOverflow)?;
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(AudioMediaError::BufferOverflow)?;
        let state = self.state.as_mut().ok_or(AudioMediaError::NotConfigured)?;
        let mut payload = [0_u8; MAX_OPUS_PAYLOAD_BYTES];
        // SAFETY: this uniquely owned state is configured for precisely this PCM
        // length (samples * channels). Both slices live for this synchronous call.
        // Native output capacity is the real fixed array size; no pointer escapes.
        let length = unsafe {
            ffi::opus_encode(
                state.ptr.as_ptr(),
                pcm.samples().as_ptr(),
                i32::from(samples),
                payload.as_mut_ptr(),
                1275,
            )
        };
        let Some(length) = usize::try_from(length)
            .ok()
            .filter(|&n| n > 0 && n <= payload.len())
        else {
            self.close();
            return Err(AudioMediaError::Fatal);
        };
        self.pending = Some(AudioAccessUnit::new(
            config.direction(),
            config.generation(),
            self.next_sequence,
            pcm.timestamp_samples(),
            samples,
            pcm.samples().iter().all(|&sample| sample == 0),
            &payload[..length],
        )?);
        self.next_sequence = next;
        self.last_end = Some(end);
        Ok(())
    }

    fn poll_packet(&mut self) -> Result<Option<AudioAccessUnit>, AudioMediaError> {
        self.config.ok_or(AudioMediaError::NotConfigured)?;
        Ok(self.pending.take())
    }

    fn configuration(&self) -> Option<AudioStreamConfig> {
        self.config
    }
}
