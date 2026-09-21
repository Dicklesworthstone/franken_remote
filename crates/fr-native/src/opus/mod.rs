//! Synchronous, thread-confined libopus adapters for an audio worker.
//!
//! This boundary does not authorize capture/playback or load peer-selected
//! libraries. The caller must keep these calls off input-authority and realtime
//! audio threads. The opt-in Linux feature
//! links the system SONAME; deployment must provide a trusted native package.
//! Codec state and pending output are bounded separately. No callback or borrowed
//! Rust input survives a native call. See `NATIVE_OPUS.md` for the ABI/trust limits.

mod decoder;
mod encoder;
mod ffi;

#[cfg(feature = "linux-audio-playout")]
pub mod playout;

pub use decoder::{Decoder, MAX_CONCEALED_SAMPLES};
pub use encoder::Encoder;

use fr_core::audio::{AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioMediaError;

/// Maximum contiguous native state allocation, checked before native creation.
/// Libopus may change its state size; an oversized implementation refuses.
pub const MAX_CODEC_STATE_BYTES: usize = 256 * 1024;

fn check_generation(
    previous: Option<AudioGeneration>,
    next: AudioGeneration,
) -> Result<(), AudioMediaError> {
    if previous.is_some_and(|old| next.as_raw() <= old.as_raw()) {
        return Err(AudioMediaError::InvalidPayload);
    }
    Ok(())
}

fn frame_samples(config: AudioStreamConfig) -> Result<u16, AudioMediaError> {
    // The current shared configuration uses integer milliseconds, so 2.5 ms is
    // not representable. Larger aggregate packets are not this encoder profile.
    if !matches!(config.frame_duration_ms(), 5 | 10 | 20 | 40 | 60) {
        return Err(AudioMediaError::UnsupportedFormat);
    }
    u16::try_from(config.expected_samples_per_frame()).map_err(|_| AudioMediaError::BufferOverflow)
}

fn state_bytes(bytes: i32) -> Result<usize, AudioMediaError> {
    usize::try_from(bytes)
        .ok()
        .filter(|&n| n > 0 && n <= MAX_CODEC_STATE_BYTES)
        .ok_or(AudioMediaError::BufferOverflow)
}

/// Codec limits supplied after session negotiation. Construction validates only
/// resource values; it does not grant a peer permission to capture or play sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecLimits {
    packet_bytes: usize,
    decoded_samples: u32,
}
impl CodecLimits {
    pub const ABSOLUTE: Self = Self {
        packet_bytes: fr_core::audio::MAX_OPUS_PAYLOAD_BYTES,
        decoded_samples: fr_core::audio::MAX_DECODED_SAMPLES,
    };

    pub fn new(packet_bytes: usize, decoded_samples: u32) -> Result<Self, AudioMediaError> {
        if packet_bytes == 0
            || packet_bytes > fr_core::audio::MAX_OPUS_PAYLOAD_BYTES
            || decoded_samples == 0
            || decoded_samples > fr_core::audio::MAX_DECODED_SAMPLES
        {
            return Err(AudioMediaError::BufferOverflow);
        }
        Ok(Self {
            packet_bytes,
            decoded_samples,
        })
    }
    pub fn max_packet_bytes(self) -> usize {
        self.packet_bytes
    }
    pub fn max_decoded_samples(self) -> u32 {
        self.decoded_samples
    }
}
