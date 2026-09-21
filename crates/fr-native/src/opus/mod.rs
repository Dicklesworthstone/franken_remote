//! Synchronous, thread-confined libopus adapters for an audio worker.
//!
//! This boundary does not authorize capture/playback, run on an input-authority
//! thread, or load libraries from peer-selected paths. The opt-in Linux feature
//! links the system SONAME; deployment must provide a trusted native package.
//! Codec state and pending output are bounded separately. No callback or borrowed
//! Rust input survives a native call. See `NATIVE_OPUS.md` for the ABI/trust limits.

mod encoder;
mod ffi;

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
