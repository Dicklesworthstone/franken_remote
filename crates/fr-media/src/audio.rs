#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_range_loop,
    clippy::collapsible_if,
    clippy::large_stack_arrays,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]
//! Core audio media pipeline, resampling boundary, and codec traits.
//!
//! Conforms to plan section 15.4 and PROTOCOL.md:
//! - Opus is the sole audio format in both directions.
//! - Fixed 48 kHz output sample rate.
//! - Channel downmixing / upmixing to Mono (1) or Stereo (2).
//! - Linear-interpolating resampler with phase accumulator for arbitrary input device rates.
//! - Bounded PCM frames with zero allocations in the hot path.
//! - [`AudioEncoder`] and [`AudioDecoder`] contracts with typed backpressure and error states.
//! - [`SyntheticAudioEncoder`] and [`SyntheticAudioDecoder`] test doubles for deterministic testing and qualification.

use fr_core::audio::{
    AudioChannels, AudioConfigError, AudioDirection, AudioGeneration, AudioStreamConfig,
    MAX_DECODED_SAMPLES, MAX_OPUS_PAYLOAD_BYTES, OPUS_SAMPLE_RATE,
};
use core::fmt;

/// Maximum number of interleaved samples stored in an [`AudioPcmFrame`] (stereo at max duration).
pub const MAX_PCM_BUFFER_SAMPLES: usize = (MAX_DECODED_SAMPLES as usize) * 2;

/// Typed audio pipeline error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMediaError {
    NotConfigured,
    Backpressure,
    NeedMoreInput,
    GenerationMismatch {
        expected: AudioGeneration,
        found: AudioGeneration,
    },
    InvalidPayload,
    BufferOverflow,
    UnsupportedFormat,
    ConfigError(AudioConfigError),
    Fatal,
}

impl fmt::Display for AudioMediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "audio codec not configured"),
            Self::Backpressure => write!(f, "audio codec backpressure: drain output first"),
            Self::NeedMoreInput => write!(f, "audio codec requires more input samples"),
            Self::GenerationMismatch { expected, found } => {
                write!(
                    f,
                    "audio generation mismatch: expected {expected:?}, found {found:?}"
                )
            }
            Self::InvalidPayload => write!(f, "invalid audio payload or packet framing"),
            Self::BufferOverflow => write!(f, "audio buffer capacity exceeded"),
            Self::UnsupportedFormat => write!(f, "unsupported native audio sample format"),
            Self::ConfigError(err) => write!(f, "audio config error: {err}"),
            Self::Fatal => write!(f, "fatal unrecoverable audio pipeline error"),
        }
    }
}

impl core::error::Error for AudioMediaError {}

impl From<AudioConfigError> for AudioMediaError {
    fn from(err: AudioConfigError) -> Self {
        Self::ConfigError(err)
    }
}

/// Native sample format produced by platform capture devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioSampleFormat {
    /// Signed 16-bit little-endian PCM.
    I16Le,
    /// Signed 24-bit little-endian PCM (3 bytes packed).
    I24Le,
    /// Signed 32-bit little-endian PCM.
    I32Le,
    /// 32-bit IEEE-754 floating point (-1.0 to 1.0).
    F32Le,
}

impl AudioSampleFormat {
    /// Number of bytes per sample in this format.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16Le => 2,
            Self::I24Le => 3,
            Self::I32Le | Self::F32Le => 4,
        }
    }

    /// Converts a raw little-endian byte slice for one sample into a normalized `f32` in `[-1.0, 1.0]`.
    pub fn decode_sample_f32(self, bytes: &[u8]) -> Result<f32, AudioMediaError> {
        if bytes.len() < self.bytes_per_sample() {
            return Err(AudioMediaError::BufferOverflow);
        }
        match self {
            Self::I16Le => {
                let s = i16::from_le_bytes([bytes[0], bytes[1]]);
                Ok(f32::from(s) / 32768.0)
            }
            Self::I24Le => {
                // Sign-extend 24-bit to 32-bit
                let sign_extend = if bytes[2] & 0x80 != 0 { 0xFF } else { 0x00 };
                let s = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign_extend]);
                Ok((s as f32) / 8_388_608.0)
            }
            Self::I32Le => {
                let s = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                Ok((s as f32) / 2_147_483_648.0)
            }
            Self::F32Le => {
                let val = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                // Clamp to prevent non-finite or extreme values from corrupting pipeline
                if val.is_finite() {
                    Ok(val.clamp(-1.0, 1.0))
                } else {
                    Ok(0.0)
                }
            }
        }
    }

    /// Converts a raw little-endian byte slice for one sample into a normalized `i16` in `[-32768, 32767]`.
    pub fn decode_sample_i16(self, bytes: &[u8]) -> Result<i16, AudioMediaError> {
        let f = self.decode_sample_f32(bytes)?;
        let scaled = f * 32767.0;
        let clamped = scaled.clamp(-32768.0, 32767.0);
        Ok(clamped as i16)
    }
}

/// Bounded, allocation-free PCM frame containing interleaved 16-bit audio samples.
#[derive(Clone, Copy)]
pub struct AudioPcmFrame {
    generation: AudioGeneration,
    channels: AudioChannels,
    sample_rate: u32,
    samples_per_channel: usize,
    timestamp_samples: u64,
    samples: [i16; MAX_PCM_BUFFER_SAMPLES],
}

impl fmt::Debug for AudioPcmFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioPcmFrame")
            .field("generation", &self.generation)
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .field("samples_per_channel", &self.samples_per_channel)
            .field("timestamp_samples", &self.timestamp_samples)
            .field("total_samples", &self.total_samples())
            .finish_non_exhaustive()
    }
}

impl AudioPcmFrame {
    /// Creates an empty frame with the given generation and channel layout.
    #[must_use]
    pub const fn empty(generation: AudioGeneration, channels: AudioChannels) -> Self {
        Self {
            generation,
            channels,
            sample_rate: OPUS_SAMPLE_RATE,
            samples_per_channel: 0,
            timestamp_samples: 0,
            samples: [0; MAX_PCM_BUFFER_SAMPLES],
        }
    }

    /// Populates an audio frame with interleaved 16-bit PCM samples.
    pub fn from_interleaved(
        generation: AudioGeneration,
        channels: AudioChannels,
        timestamp_samples: u64,
        source: &[i16],
    ) -> Result<Self, AudioMediaError> {
        let ch_count = channels.count() as usize;
        if source.is_empty() || !source.len().is_multiple_of(ch_count) {
            return Err(AudioMediaError::InvalidPayload);
        }
        let samples_per_channel = source.len() / ch_count;
        if samples_per_channel > (MAX_DECODED_SAMPLES as usize) {
            return Err(AudioMediaError::BufferOverflow);
        }

        let mut frame = Self::empty(generation, channels);
        frame.samples_per_channel = samples_per_channel;
        frame.timestamp_samples = timestamp_samples;
        frame.samples[..source.len()].copy_from_slice(source);
        Ok(frame)
    }

    #[must_use]
    pub const fn generation(&self) -> AudioGeneration {
        self.generation
    }

    #[must_use]
    pub const fn channels(&self) -> AudioChannels {
        self.channels
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub const fn samples_per_channel(&self) -> usize {
        self.samples_per_channel
    }

    #[must_use]
    pub const fn total_samples(&self) -> usize {
        self.samples_per_channel * (self.channels.count() as usize)
    }

    #[must_use]
    pub const fn timestamp_samples(&self) -> u64 {
        self.timestamp_samples
    }

    /// Read-only access to interleaved sample buffer.
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        &self.samples[..self.total_samples()]
    }

    /// Mutable access to interleaved sample buffer (e.g. for software gain or attenuation).
    pub fn samples_mut(&mut self) -> &mut [i16] {
        let total = self.total_samples();
        &mut self.samples[..total]
    }

    /// Computes root-mean-square (RMS) energy across all samples in this frame.
    #[must_use]
    pub fn rms_energy(&self) -> f32 {
        let total = self.total_samples();
        if total == 0 {
            return 0.0;
        }
        let mut sum_sq: f64 = 0.0;
        for &s in self.samples() {
            let normalized = f64::from(s) / 32768.0;
            sum_sq += normalized * normalized;
        }
        ((sum_sq / (total as f64)).sqrt()) as f32
    }
}

/// Encoded Opus access unit ready for wire delivery or client decoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AudioAccessUnit {
    direction: AudioDirection,
    generation: AudioGeneration,
    sequence: u64,
    timestamp_samples: u64,
    duration_samples: u16,
    is_silence: bool,
    payload_len: usize,
    payload: [u8; MAX_OPUS_PAYLOAD_BYTES],
}

impl fmt::Debug for AudioAccessUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioAccessUnit")
            .field("direction", &self.direction)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("timestamp_samples", &self.timestamp_samples)
            .field("duration_samples", &self.duration_samples)
            .field("is_silence", &self.is_silence)
            .field("payload_len", &self.payload_len)
            .finish_non_exhaustive()
    }
}

impl AudioAccessUnit {
    /// Creates a new access unit from a byte payload.
    pub fn new(
        direction: AudioDirection,
        generation: AudioGeneration,
        sequence: u64,
        timestamp_samples: u64,
        duration_samples: u16,
        is_silence: bool,
        payload_data: &[u8],
    ) -> Result<Self, AudioMediaError> {
        if payload_data.is_empty() || payload_data.len() > MAX_OPUS_PAYLOAD_BYTES {
            return Err(AudioMediaError::InvalidPayload);
        }
        if duration_samples == 0 || u32::from(duration_samples) > MAX_DECODED_SAMPLES {
            return Err(AudioMediaError::InvalidPayload);
        }

        let mut unit = Self {
            direction,
            generation,
            sequence,
            timestamp_samples,
            duration_samples,
            is_silence,
            payload_len: payload_data.len(),
            payload: [0; MAX_OPUS_PAYLOAD_BYTES],
        };
        unit.payload[..payload_data.len()].copy_from_slice(payload_data);
        Ok(unit)
    }

    #[must_use]
    pub const fn direction(&self) -> AudioDirection {
        self.direction
    }

    #[must_use]
    pub const fn generation(&self) -> AudioGeneration {
        self.generation
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn timestamp_samples(&self) -> u64 {
        self.timestamp_samples
    }

    #[must_use]
    pub const fn duration_samples(&self) -> u16 {
        self.duration_samples
    }

    #[must_use]
    pub const fn is_silence(&self) -> bool {
        self.is_silence
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }
}

/// Controlled resampler and channel converter boundary (Plan §15.4).
///
/// Converts arbitrary native device rates (e.g. 44,100 Hz, 88,200 Hz, 96,000 Hz)
/// and input channels (1..=8) to canonical 48,000 Hz Mono or Stereo frames.
/// Zero dynamic allocations occur during streaming conversion.
pub struct AudioResampler {
    input_sample_rate: u32,
    input_channels: u8,
    output_channels: AudioChannels,
    phase_num: u64,
    step_num: u64,
    step_den: u64,
}

impl AudioResampler {
    /// Creates a resampler converting from `input_sample_rate` and `input_channels`
    /// to 48 kHz `output_channels`.
    pub fn new(
        input_sample_rate: u32,
        input_channels: u8,
        output_channels: AudioChannels,
    ) -> Result<Self, AudioMediaError> {
        if input_sample_rate == 0 || input_sample_rate > 384_000 {
            return Err(AudioMediaError::UnsupportedFormat);
        }
        if input_channels == 0 || input_channels > 8 {
            return Err(AudioMediaError::UnsupportedFormat);
        }

        let step_num = u64::from(input_sample_rate);
        let step_den = u64::from(OPUS_SAMPLE_RATE);
        Ok(Self {
            input_sample_rate,
            input_channels,
            output_channels,
            phase_num: 0,
            step_num,
            step_den,
        })
    }

    #[must_use]
    pub const fn input_sample_rate(&self) -> u32 {
        self.input_sample_rate
    }

    #[must_use]
    pub const fn input_channels(&self) -> u8 {
        self.input_channels
    }

    /// Resets resampler phase on device switch or stream reset.
    pub fn reset(&mut self) {
        self.phase_num = 0;
    }

    /// Resamples an input slice of interleaved normalized `f32` samples into
    /// standard 48 kHz `AudioPcmFrame` output.
    pub fn resample_f32(
        &mut self,
        generation: AudioGeneration,
        timestamp_samples: u64,
        input_interleaved: &[f32],
        output_frame: &mut AudioPcmFrame,
    ) -> Result<usize, AudioMediaError> {
        let in_ch = self.input_channels as usize;
        if input_interleaved.is_empty() || !input_interleaved.len().is_multiple_of(in_ch) {
            return Err(AudioMediaError::InvalidPayload);
        }
        let in_frames = input_interleaved.len() / in_ch;

        let out_ch_count = self.output_channels.count() as usize;
        let mut out_idx = 0;
        let max_out_samples_per_ch = MAX_DECODED_SAMPLES as usize;

        // Process each required output sample at 48 kHz using exact rational phase
        while (self.phase_num / self.step_den) < (in_frames as u64) {
            let in_idx = (self.phase_num / self.step_den) as usize;
            let frac = ((self.phase_num % self.step_den) as f32) / (self.step_den as f32);

            if out_idx >= max_out_samples_per_ch {
                break;
            }

            // Interpolate per input channel
            let mut curr_channels = [0.0f32; 8];
            for c in 0..in_ch {
                let curr_val = input_interleaved[in_idx * in_ch + c];
                let next_val = if in_idx + 1 < in_frames {
                    input_interleaved[(in_idx + 1) * in_ch + c]
                } else {
                    curr_val
                };
                curr_channels[c] = curr_val + frac * (next_val - curr_val);
            }

            // Downmix / Upmix to target channels
            match (in_ch, self.output_channels) {
                (1, AudioChannels::Mono) => {
                    let s = (curr_channels[0] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    output_frame.samples[out_idx] = s;
                }
                (1, AudioChannels::Stereo) => {
                    let s = (curr_channels[0] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    output_frame.samples[out_idx * 2] = s;
                    output_frame.samples[out_idx * 2 + 1] = s;
                }
                (2, AudioChannels::Mono) => {
                    let mono = f32::midpoint(curr_channels[0], curr_channels[1]);
                    let s = (mono * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    output_frame.samples[out_idx] = s;
                }
                (2, AudioChannels::Stereo) => {
                    let l = (curr_channels[0] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    let r = (curr_channels[1] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    output_frame.samples[out_idx * 2] = l;
                    output_frame.samples[out_idx * 2 + 1] = r;
                }
                (6, AudioChannels::Stereo) => {
                    // 5.1 surround: L, R, C, LFE, Ls, Rs -> Stereo L, R
                    let l = curr_channels[0] + 0.707 * curr_channels[2] + 0.707 * curr_channels[4];
                    let r = curr_channels[1] + 0.707 * curr_channels[2] + 0.707 * curr_channels[5];
                    let l_i16 = ((l * 0.5) * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    let r_i16 = ((r * 0.5) * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    output_frame.samples[out_idx * 2] = l_i16;
                    output_frame.samples[out_idx * 2 + 1] = r_i16;
                }
                _ => {
                    // General downmix: average all channels for mono/stereo
                    let mut sum = 0.0f32;
                    for c in 0..in_ch {
                        sum += curr_channels[c];
                    }
                    let avg = sum / (in_ch as f32);
                    let s = (avg * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    if self.output_channels == AudioChannels::Mono {
                        output_frame.samples[out_idx] = s;
                    } else {
                        output_frame.samples[out_idx * 2] = s;
                        output_frame.samples[out_idx * 2 + 1] = s;
                    }
                }
            }

            out_idx += 1;
            self.phase_num += self.step_num;
        }

        // Adjust phase accumulator relative to current chunk end
        self.phase_num = self.phase_num.saturating_sub((in_frames as u64) * self.step_den);

        output_frame.generation = generation;
        output_frame.channels = self.output_channels;
        output_frame.sample_rate = OPUS_SAMPLE_RATE;
        output_frame.samples_per_channel = out_idx;
        output_frame.timestamp_samples = timestamp_samples;

        Ok(out_idx * out_ch_count)
    }
}

/// Voice activity / silence detection helper.
pub struct AudioSilenceDetector {
    threshold_rms: f32,
    hangover_frames: u16,
    hangover_counter: u16,
}

impl Default for AudioSilenceDetector {
    fn default() -> Self {
        Self {
            // ~ -60 dBFS
            threshold_rms: 0.001,
            // 5 frames (50 ms) hangover to avoid chopping speech tails
            hangover_frames: 5,
            hangover_counter: 0,
        }
    }
}

impl AudioSilenceDetector {
    #[must_use]
    pub fn new(threshold_rms: f32, hangover_frames: u16) -> Self {
        Self {
            threshold_rms,
            hangover_frames,
            hangover_counter: 0,
        }
    }

    /// Evaluates a PCM frame. Returns `true` if the frame is deemed silent.
    pub fn is_silence(&mut self, frame: &AudioPcmFrame) -> bool {
        let energy = frame.rms_energy();
        if energy >= self.threshold_rms {
            self.hangover_counter = self.hangover_frames;
            false
        } else if self.hangover_counter > 0 {
            self.hangover_counter -= 1;
            false
        } else {
            true
        }
    }
}

/// Trait modeling an Opus audio encoder.
pub trait AudioEncoder {
    /// Configures the encoder with the negotiated stream parameters.
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError>;

    /// Submits a PCM frame for encoding.
    fn submit_pcm(&mut self, pcm: &AudioPcmFrame) -> Result<(), AudioMediaError>;

    /// Drains the next encoded access unit, if ready.
    fn poll_packet(&mut self) -> Result<Option<AudioAccessUnit>, AudioMediaError>;

    /// Current stream configuration, if configured.
    fn configuration(&self) -> Option<AudioStreamConfig>;
}

/// Trait modeling an Opus audio decoder.
pub trait AudioDecoder {
    /// Configures the decoder with the negotiated stream parameters.
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError>;

    /// Submits an encoded access unit for decoding.
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError>;

    /// Requests Packet Loss Concealment (PLC) for a lost packet of given sample duration.
    fn decode_plc(&mut self, duration_samples: u16) -> Result<AudioPcmFrame, AudioMediaError>;

    /// Drains the next decoded PCM frame, if ready.
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError>;

    /// Resets decoder state and advances generation (e.g. after reconnect or device change).
    fn reset(&mut self, new_generation: AudioGeneration);
}

/// Deterministic synthetic audio codec test double.
///
/// Deterministic synthetic audio encoder test double.
pub struct SyntheticAudioEncoder {
    config: Option<AudioStreamConfig>,
    encoder_sequence: u64,
    pending_packets: [Option<AudioAccessUnit>; 8],
    pending_packet_count: usize,
}

impl Default for SyntheticAudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticAudioEncoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            encoder_sequence: 0,
            pending_packets: [None, None, None, None, None, None, None, None],
            pending_packet_count: 0,
        }
    }
}

impl AudioEncoder for SyntheticAudioEncoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(config);
        self.encoder_sequence = 0;
        self.pending_packet_count = 0;
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
        if self.pending_packet_count >= self.pending_packets.len() {
            return Err(AudioMediaError::Backpressure);
        }

        let seq = self.encoder_sequence;
        self.encoder_sequence += 1;

        // Create synthetic Opus payload: 16 bytes
        let mut payload = [0u8; 16];
        payload[0..8].copy_from_slice(&seq.to_le_bytes());
        let duration = pcm.samples_per_channel() as u16;
        payload[8..10].copy_from_slice(&duration.to_le_bytes());
        let peak_sample = pcm.samples().first().copied().unwrap_or(0);
        payload[10..12].copy_from_slice(&peak_sample.to_le_bytes());
        // Simple checksum
        payload[12] = 0xAA;
        payload[13] = 0x55;
        payload[14] = config.channels() as u8;
        payload[15] = 0x01;

        let unit = AudioAccessUnit::new(
            config.direction(),
            config.generation(),
            seq,
            pcm.timestamp_samples(),
            duration,
            pcm.rms_energy() < 0.001,
            &payload,
        )?;

        self.pending_packets[self.pending_packet_count] = Some(unit);
        self.pending_packet_count += 1;
        Ok(())
    }

    fn poll_packet(&mut self) -> Result<Option<AudioAccessUnit>, AudioMediaError> {
        if self.pending_packet_count == 0 {
            return Ok(None);
        }
        let unit = self.pending_packets[0].take();
        for i in 1..self.pending_packet_count {
            self.pending_packets[i - 1] = self.pending_packets[i].take();
        }
        self.pending_packet_count -= 1;
        Ok(unit)
    }

    fn configuration(&self) -> Option<AudioStreamConfig> {
        self.config
    }
}

/// Deterministic synthetic audio decoder test double.
pub struct SyntheticAudioDecoder {
    config: Option<AudioStreamConfig>,
    pending_pcm: [Option<AudioPcmFrame>; 8],
    pending_pcm_count: usize,
    last_decoded_sample: i16,
}

impl Default for SyntheticAudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticAudioDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            pending_pcm: [None, None, None, None, None, None, None, None],
            pending_pcm_count: 0,
            last_decoded_sample: 0,
        }
    }
}

impl AudioDecoder for SyntheticAudioDecoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(config);
        self.pending_pcm_count = 0;
        self.last_decoded_sample = 0;
        Ok(())
    }

    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        let config = self.config.ok_or(AudioMediaError::NotConfigured)?;
        if packet.generation() != config.generation() {
            return Err(AudioMediaError::GenerationMismatch {
                expected: config.generation(),
                found: packet.generation(),
            });
        }
        if self.pending_pcm_count >= self.pending_pcm.len() {
            return Err(AudioMediaError::Backpressure);
        }

        let payload = packet.payload();
        if payload.len() < 16 || payload[12] != 0xAA || payload[13] != 0x55 {
            return Err(AudioMediaError::InvalidPayload);
        }

        let peak_sample = i16::from_le_bytes([payload[10], payload[11]]);
        self.last_decoded_sample = peak_sample;

        let duration = packet.duration_samples() as usize;
        let mut frame = AudioPcmFrame::empty(config.generation(), config.channels());
        frame.samples_per_channel = duration;
        frame.timestamp_samples = packet.timestamp_samples();

        let total_samples = duration * (config.channels().count() as usize);
        for i in 0..total_samples {
            frame.samples[i] = peak_sample;
        }

        self.pending_pcm[self.pending_pcm_count] = Some(frame);
        self.pending_pcm_count += 1;
        Ok(())
    }

    fn decode_plc(&mut self, duration_samples: u16) -> Result<AudioPcmFrame, AudioMediaError> {
        let config = self.config.ok_or(AudioMediaError::NotConfigured)?;
        let mut frame = AudioPcmFrame::empty(config.generation(), config.channels());
        let count = (duration_samples as usize).min(MAX_DECODED_SAMPLES as usize);
        frame.samples_per_channel = count;

        // Packet Loss Concealment (PLC): waveform fade-out of last decoded sample
        let total = count * (config.channels().count() as usize);
        let mut current = self.last_decoded_sample;
        for i in 0..total {
            current = ((f32::from(current)) * 0.95) as i16;
            frame.samples[i] = current;
        }
        self.last_decoded_sample = current;

        Ok(frame)
    }

    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        if self.pending_pcm_count == 0 {
            return Ok(None);
        }
        let frame = self.pending_pcm[0].take();
        for i in 1..self.pending_pcm_count {
            self.pending_pcm[i - 1] = self.pending_pcm[i].take();
        }
        self.pending_pcm_count -= 1;
        Ok(frame)
    }

    fn reset(&mut self, new_generation: AudioGeneration) {
        if let Some(cfg) = &mut self.config {
            if let Ok(new_cfg) = AudioStreamConfig::new(
                cfg.direction(),
                new_generation,
                cfg.channels(),
                cfg.frame_duration_ms(),
                cfg.jitter_target_ms(),
            ) {
                *cfg = new_cfg;
            }
        }
        self.pending_pcm_count = 0;
        self.last_decoded_sample = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_and_conversions() {
        assert_eq!(AudioSampleFormat::I16Le.bytes_per_sample(), 2);
        assert_eq!(AudioSampleFormat::I24Le.bytes_per_sample(), 3);
        assert_eq!(AudioSampleFormat::I32Le.bytes_per_sample(), 4);
        assert_eq!(AudioSampleFormat::F32Le.bytes_per_sample(), 4);

        let i16_bytes = 16384i16.to_le_bytes();
        let f = AudioSampleFormat::I16Le.decode_sample_f32(&i16_bytes).unwrap();
        assert!((f - 0.5).abs() < 0.001);

        let f32_bytes = 0.5f32.to_le_bytes();
        let s = AudioSampleFormat::F32Le.decode_sample_i16(&f32_bytes).unwrap();
        assert!((s - 16383).abs() <= 1);
    }

    #[test]
    fn pcm_frame_lifecycle_and_energy() {
        let generation = AudioGeneration::INITIAL;
        let frame = AudioPcmFrame::empty(generation, AudioChannels::Stereo);
        assert_eq!(frame.samples_per_channel(), 0);
        assert_eq!(frame.rms_energy(), 0.0);

        let samples = [1000i16, -1000i16, 2000i16, -2000i16];
        let frame2 = AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 100, &samples).unwrap();
        assert_eq!(frame2.samples_per_channel(), 2);
        assert_eq!(frame2.total_samples(), 4);
        assert!(frame2.rms_energy() > 0.0);
    }

    #[test]
    fn resampler_converts_44100_to_48000() {
        let generation = AudioGeneration::INITIAL;
        let mut resampler = AudioResampler::new(44_100, 2, AudioChannels::Stereo).unwrap();
        let input_samples = vec![0.5f32; 441 * 2]; // 10 ms at 44.1 kHz
        let mut out_frame = AudioPcmFrame::empty(generation, AudioChannels::Stereo);

        let written = resampler
            .resample_f32(generation, 0, &input_samples, &mut out_frame)
            .unwrap();
        assert_eq!(written, out_frame.total_samples());
        // 480 samples per channel * 2 channels = 960 total
        assert!((out_frame.samples_per_channel() as i32 - 480).abs() <= 2);
    }

    #[test]
    fn synthetic_codec_encode_decode_round_trip() {
        let generation = AudioGeneration::INITIAL;
        let config = AudioStreamConfig::new(
            AudioDirection::Downlink,
            generation,
            AudioChannels::Stereo,
            10,
            20,
        )
        .unwrap();

        let mut encoder = SyntheticAudioEncoder::new();
        encoder.configure(config).unwrap();

        let samples = vec![1234i16; 480 * 2];
        let pcm_in = AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 0, &samples).unwrap();
        encoder.submit_pcm(&pcm_in).unwrap();

        let packet = encoder.poll_packet().unwrap().expect("packet ready");
        assert_eq!(packet.sequence(), 0);
        assert_eq!(packet.duration_samples(), 480);

        let mut decoder = SyntheticAudioDecoder::new();
        decoder.configure(config).unwrap();
        decoder.submit_packet(&packet).unwrap();

        let pcm_out = decoder.poll_pcm().unwrap().expect("pcm ready");
        assert_eq!(pcm_out.samples_per_channel(), 480);
        assert_eq!(pcm_out.samples()[0], 1234);
    }

    #[test]
    fn synthetic_codec_plc() {
        let generation = AudioGeneration::INITIAL;
        let config = AudioStreamConfig::new(
            AudioDirection::Downlink,
            generation,
            AudioChannels::Stereo,
            10,
            20,
        )
        .unwrap();

        let mut decoder = SyntheticAudioDecoder::new();
        decoder.configure(config).unwrap();

        let plc_frame = decoder.decode_plc(480).unwrap();
        assert_eq!(plc_frame.samples_per_channel(), 480);
        assert_eq!(plc_frame.samples()[0], 0);
    }
}
