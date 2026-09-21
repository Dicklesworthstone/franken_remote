use super::{CodecLimits, check_generation, ffi, state_bytes};
use fr_core::audio::{
    AudioGeneration, AudioStreamConfig, MAX_DECODED_SAMPLES, MAX_OPUS_PAYLOAD_BYTES,
};
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioMediaError, AudioPcmFrame};
use std::{ffi::c_void, fmt, ptr::NonNull};

/// At most 100 ms of consecutive explicit packet loss concealment at 48 kHz.
/// Beyond this bound, re-establish the audio generation rather than inventing
/// an indefinitely playable stream from silence or the last decoded waveform.
pub const MAX_CONCEALED_SAMPLES: u32 =
    fr_core::audio::OPUS_SAMPLE_RATE / 1000 * fr_core::audio::MAX_JITTER_CEILING_MS as u32;

struct State {
    ptr: NonNull<c_void>, // Deliberately !Send/!Sync; one exclusive worker owner.
    bytes: usize,
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of a create-returned state. No pointers
        // are retained by any asynchronous operation or another owner.
        unsafe { ffi::opus_decoder_destroy(self.ptr.as_ptr()) };
    }
}
impl State {
    fn new(config: AudioStreamConfig) -> Result<Self, AudioMediaError> {
        let channels = i32::from(config.channels().count());
        // SAFETY: bounded mono/stereo query, no borrowed buffers or state.
        let bytes = state_bytes(unsafe { ffi::opus_decoder_get_size(channels) })?;
        let mut error = 0;
        // SAFETY: supported rate/channels, valid writable status, checked result.
        let ptr = unsafe { ffi::opus_decoder_create(48_000, channels, &raw mut error) };
        let state = Self {
            ptr: NonNull::new(ptr).ok_or(AudioMediaError::Fatal)?,
            bytes,
        };
        if error != 0 {
            return Err(AudioMediaError::Fatal);
        }
        // SAFETY: the 1.5 decoder accepts the promoted integer complexity CTL.
        // Zero explicitly disables Deep PLC/enhancement. Refuse an ABI that does
        // not support this control; never silently enable a model-based path.
        if unsafe { ffi::opus_decoder_ctl(state.ptr.as_ptr(), ffi::SET_COMPLEXITY, 0_i32) } != 0 {
            return Err(AudioMediaError::UnsupportedFormat);
        }
        Ok(state)
    }
    fn decode(
        &mut self,
        packet: Option<&[u8]>,
        samples: u16,
        pcm: &mut [i16],
    ) -> Result<(), AudioMediaError> {
        let (ptr, len) =
            packet.map_or((std::ptr::null(), 0), |bytes| (bytes.as_ptr(), bytes.len()));
        let len = i32::try_from(len).map_err(|_| AudioMediaError::BufferOverflow)?;
        // SAFETY: callers validate frame size and allocate exactly that many
        // samples per configured channel. A borrowed complete packet remains
        // live for the call; null/zero is the documented PLC request, not EOF.
        // No decode-FEC guessing, retained input pointers, or shared state.
        let received = unsafe {
            ffi::opus_decode(
                self.ptr.as_ptr(),
                ptr,
                len,
                pcm.as_mut_ptr(),
                i32::from(samples),
                0,
            )
        };
        if received == i32::from(samples) {
            Ok(())
        } else if received == -4 {
            // OPUS_INVALID_PACKET
            Err(AudioMediaError::InvalidPayload)
        } else {
            Err(AudioMediaError::Fatal)
        }
    }
}

#[derive(Clone, Copy)]
struct Cursor {
    sequence: u64,
    at: u64,
}
impl Cursor {
    fn next(self, samples: u16) -> Result<Self, AudioMediaError> {
        Ok(Self {
            sequence: self
                .sequence
                .checked_add(1)
                .ok_or(AudioMediaError::BufferOverflow)?,
            at: self
                .at
                .checked_add(u64::from(samples))
                .ok_or(AudioMediaError::BufferOverflow)?,
        })
    }
}

/// Real Opus decoder with one PCM slot and explicit, sample-timed loss concealment.
///
/// The caller supplies ordered packets after bounded jitter/reassembly and checks
/// session/observation authority before calling this synchronous worker boundary.
/// Packet metadata is not trusted: full framing and actual decoded sample count
/// are checked before the stateful codec call. Duplicate/out-of-order packets and
/// unannounced duration changes are refused rather than replayed or reconfigured.
pub struct Decoder {
    state: Option<State>,
    limits: CodecLimits,
    config: Option<AudioStreamConfig>,
    generation: Option<AudioGeneration>,
    pcm: Box<[i16]>,
    pending: Option<u64>, // Timestamp of the sole retained PCM frame.
    next: Option<Cursor>,
    concealed: u32,
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("configuration", &self.config)
            .field("native_bytes", &self.native_state_bytes())
            .field("pcm_bytes", &self.pcm_capacity_bytes())
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}
impl Decoder {
    pub fn new() -> Self {
        Self::with_limits(CodecLimits::ABSOLUTE)
    }

    /// Resource ceilings from admitted negotiation, immutable for this owner.
    pub fn with_limits(limits: CodecLimits) -> Self {
        Self {
            state: None,
            limits,
            config: None,
            generation: None,
            pcm: Box::default(),
            pending: None,
            next: None,
            concealed: 0,
        }
    }
    pub fn configuration(&self) -> Option<AudioStreamConfig> {
        self.config
    }
    pub fn native_state_bytes(&self) -> usize {
        self.state.as_ref().map_or(0, |state| state.bytes)
    }
    pub fn pcm_capacity_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.pcm)
    }

    /// Release buffered sound and native state without forgetting retired epochs.
    pub fn close(&mut self) {
        self.pcm.fill(0);
        self.pcm = Box::default();
        self.pending = None;
        self.next = None;
        self.concealed = 0;
        self.config = None;
        self.state = None;
    }

    fn take_pcm(&mut self, timestamp: u64) -> Result<AudioPcmFrame, AudioMediaError> {
        let config = self.config.ok_or(AudioMediaError::NotConfigured)?;
        let frame = AudioPcmFrame::from_interleaved(
            config.generation(),
            config.channels(),
            timestamp,
            &self.pcm,
        );
        self.pcm.fill(0);
        frame
    }
}
impl AudioDecoder for Decoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        if self.pending.is_some() {
            return Err(AudioMediaError::Backpressure);
        }
        let samples = decoded_samples(config)?;
        if u32::from(samples) > self.limits.max_decoded_samples() {
            return Err(AudioMediaError::BufferOverflow);
        }
        check_generation(self.generation, config.generation())?;
        let count = usize::from(samples)
            .checked_mul(usize::from(config.channels().count()))
            .ok_or(AudioMediaError::BufferOverflow)?;
        let mut pcm = Vec::new();
        pcm.try_reserve_exact(count)
            .map_err(|_| AudioMediaError::BufferOverflow)?;
        pcm.resize(count, 0_i16);
        let state = State::new(config)?;
        self.pcm = pcm.into_boxed_slice();
        self.state = Some(state);
        self.config = Some(config);
        self.generation = Some(config.generation());
        self.next = None;
        self.concealed = 0;
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
        if self.pending.is_some() {
            return Err(AudioMediaError::Backpressure);
        }
        let samples = decoded_samples(config)?;
        if packet.direction() != config.direction()
            || packet.duration_samples() != samples
            || self.next.is_some_and(|next| {
                packet.sequence() != next.sequence || packet.timestamp_samples() < next.at
            })
        {
            return Err(AudioMediaError::InvalidPayload);
        }
        let next = Cursor {
            sequence: packet.sequence(),
            at: packet.timestamp_samples(),
        }
        .next(samples)?;
        if packet.payload().len() > self.limits.max_packet_bytes() {
            return Err(AudioMediaError::BufferOverflow);
        }
        validate_packet(packet.payload(), samples)?;
        let state = self.state.as_mut().ok_or(AudioMediaError::NotConfigured)?;
        if let Err(error) = state.decode(Some(packet.payload()), samples, &mut self.pcm) {
            // A failed native call may have changed codec history. Never reuse it.
            self.close();
            return Err(error);
        }
        self.pending = Some(packet.timestamp_samples());
        self.next = Some(next);
        self.concealed = 0;
        Ok(())
    }

    fn decode_plc(&mut self, duration_samples: u16) -> Result<AudioPcmFrame, AudioMediaError> {
        let config = self.config.ok_or(AudioMediaError::NotConfigured)?;
        if self.pending.is_some() {
            return Err(AudioMediaError::Backpressure);
        }
        let cursor = self.next.ok_or(AudioMediaError::NeedMoreInput)?;
        if duration_samples != decoded_samples(config)? {
            return Err(AudioMediaError::InvalidPayload);
        }
        let concealed = self
            .concealed
            .checked_add(u32::from(duration_samples))
            .filter(|&samples| samples <= MAX_CONCEALED_SAMPLES)
            .ok_or(AudioMediaError::NeedMoreInput)?;
        let next = cursor.next(duration_samples)?;
        let state = self.state.as_mut().ok_or(AudioMediaError::NotConfigured)?;
        if let Err(error) = state.decode(None, duration_samples, &mut self.pcm) {
            self.close();
            return Err(error);
        }
        self.next = Some(next);
        self.concealed = concealed;
        self.take_pcm(cursor.at)
    }

    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        self.config.ok_or(AudioMediaError::NotConfigured)?;
        self.pending
            .take()
            .map(|timestamp| self.take_pcm(timestamp))
            .transpose()
    }

    fn reset(&mut self, generation: AudioGeneration) {
        // The existing trait has no error return. A stale reset therefore closes
        // the owner, never silently keeps buffered old sound or relabels history.
        if check_generation(self.generation, generation).is_err() {
            self.close();
            return;
        }
        self.generation = Some(generation);
        self.pending = None;
        self.next = None;
        self.concealed = 0;
        self.pcm.fill(0);
        let (Some(config), Some(state)) = (self.config, self.state.as_mut()) else {
            return;
        };
        // SAFETY: no additional argument belongs to RESET_STATE; the native
        // allocation is live and exclusive. Reset does not construct a new owner.
        if unsafe { ffi::opus_decoder_ctl(state.ptr.as_ptr(), ffi::RESET_STATE) } != 0 {
            self.close();
            return;
        }
        self.config = AudioStreamConfig::new(
            config.direction(),
            generation,
            config.channels(),
            config.frame_duration_ms(),
            config.jitter_target_ms(),
        )
        .ok();
        if self.config.is_none() {
            self.close();
        }
    }
}

fn decoded_samples(config: AudioStreamConfig) -> Result<u16, AudioMediaError> {
    if !matches!(
        config.frame_duration_ms(),
        5 | 10 | 20 | 40 | 60 | 80 | 100 | 120
    ) {
        return Err(AudioMediaError::UnsupportedFormat);
    }
    u16::try_from(config.expected_samples_per_frame()).map_err(|_| AudioMediaError::BufferOverflow)
}

fn validate_packet(bytes: &[u8], samples: u16) -> Result<(), AudioMediaError> {
    if bytes.is_empty() || bytes.len() > MAX_OPUS_PAYLOAD_BYTES {
        return Err(AudioMediaError::InvalidPayload);
    }
    let length = i32::try_from(bytes.len()).map_err(|_| AudioMediaError::BufferOverflow)?;
    let mut toc = 0_u8;
    let mut frames = [std::ptr::null(); 48];
    let mut sizes = [0_i16; 48];
    let mut offset = 0_i32;
    // SAFETY: the complete input is bounded by its real length. These are the
    // documented 48-element writable arrays for packet_parse. Returned pointers
    // remain unused and are not retained; the query never touches decoder state.
    let (count, actual_samples) = unsafe {
        (
            ffi::opus_packet_parse(
                bytes.as_ptr(),
                length,
                &raw mut toc,
                frames.as_mut_ptr(),
                sizes.as_mut_ptr(),
                &raw mut offset,
            ),
            ffi::opus_packet_get_nb_samples(bytes.as_ptr(), length, 48_000),
        )
    };
    if !(1..=48).contains(&count)
        || actual_samples != i32::from(samples)
        || u32::from(samples) > MAX_DECODED_SAMPLES
    {
        return Err(AudioMediaError::InvalidPayload);
    }
    Ok(())
}
