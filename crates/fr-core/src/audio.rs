#![forbid(unsafe_code)]
//! Core Opus audio types, format constants, and limits.
//!
//! Per plan section 15.4 and PROTOCOL.md:
//! - Opus is the sole audio format in both directions.
//! - Fixed 48,000 Hz sample rate.
//! - Channels: mono (1) or stereo (2).
//! - Nominal 10 ms packets (480 samples/channel).
//! - Maximum decoded samples and packet sizes are validated before allocation.
//! - Audio generations fence device switches and reconnects.

pub use crate::ids::AudioGeneration;
use core::fmt;

/// Fixed sample rate for all `FrankenRemote` audio pipelines (48 kHz).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Nominal packet duration in milliseconds.
pub const NOMINAL_PACKET_DURATION_MS: u16 = 10;

/// Number of samples per channel in a nominal 10 ms packet at 48 kHz.
pub const NOMINAL_SAMPLES_PER_FRAME: u16 = 480;

/// Maximum packet duration in milliseconds per RFC 6716.
pub const MAX_PACKET_DURATION_MS: u16 = 120;

/// Maximum decoded samples per channel in one packet (120 ms at 48 kHz).
pub const MAX_DECODED_SAMPLES: u32 = 5_760;

/// Maximum Opus payload size in bytes per RFC 6716.
pub const MAX_OPUS_PAYLOAD_BYTES: usize = 1_275;

/// Strict ceiling for client-side jitter buffer in milliseconds.
/// Late packets arriving beyond this window are discarded immediately.
pub const MAX_JITTER_CEILING_MS: u16 = 100;

/// Default target jitter delay in milliseconds.
pub const DEFAULT_JITTER_TARGET_MS: u16 = 20;

/// Mandatory disclosure for Windows audio capture per plan section 15.4.
pub const WINDOWS_ENDPOINT_SCOPE_DISCLOSURE: &str =
    "Windows endpoint loopback captures system-wide audio across all terminal sessions, not isolated to the selected desktop user or application.";

/// Audio transmission direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AudioDirection {
    /// Host playback audio sent to client.
    Downlink = 0,
    /// Client microphone audio sent to host.
    Uplink = 1,
}

impl AudioDirection {
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Downlink),
            1 => Some(Self::Uplink),
            _ => None,
        }
    }
}

/// Negotiated channel configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AudioChannels {
    Mono = 1,
    Stereo = 2,
}

impl AudioChannels {
    #[must_use]
    pub const fn count(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Mono),
            2 => Some(Self::Stereo),
            _ => None,
        }
    }
}

/// Reason for audio stream termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AudioStopReason {
    UserMute = 1,
    SessionEnded = 2,
    DeviceChanged = 3,
    BufferOverflow = 4,
    HostDisabled = 5,
}

impl AudioStopReason {
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::UserMute),
            2 => Some(Self::SessionEnded),
            3 => Some(Self::DeviceChanged),
            4 => Some(Self::BufferOverflow),
            5 => Some(Self::HostDisabled),
            _ => None,
        }
    }
}

/// Validated audio stream parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStreamConfig {
    direction: AudioDirection,
    generation: AudioGeneration,
    channels: AudioChannels,
    frame_duration_ms: u16,
    jitter_target_ms: u16,
}

/// Error returned when audio configuration parameters exceed bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioConfigError {
    InvalidDuration,
    InvalidJitterTarget,
    ZeroChannels,
}

impl fmt::Display for AudioConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDuration => write!(f, "invalid packet duration; max is {MAX_PACKET_DURATION_MS} ms"),
            Self::InvalidJitterTarget => write!(f, "jitter target exceeds ceiling of {MAX_JITTER_CEILING_MS} ms"),
            Self::ZeroChannels => write!(f, "channels must be 1 (mono) or 2 (stereo)"),
        }
    }
}

impl core::error::Error for AudioConfigError {}

impl AudioStreamConfig {
    /// Creates and validates a stream configuration.
    pub fn new(
        direction: AudioDirection,
        generation: AudioGeneration,
        channels: AudioChannels,
        frame_duration_ms: u16,
        jitter_target_ms: u16,
    ) -> Result<Self, AudioConfigError> {
        if frame_duration_ms == 0 || frame_duration_ms > MAX_PACKET_DURATION_MS {
            return Err(AudioConfigError::InvalidDuration);
        }
        if jitter_target_ms == 0 || jitter_target_ms > MAX_JITTER_CEILING_MS {
            return Err(AudioConfigError::InvalidJitterTarget);
        }
        Ok(Self {
            direction,
            generation,
            channels,
            frame_duration_ms,
            jitter_target_ms,
        })
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
    pub const fn channels(&self) -> AudioChannels {
        self.channels
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        OPUS_SAMPLE_RATE
    }

    #[must_use]
    pub const fn frame_duration_ms(&self) -> u16 {
        self.frame_duration_ms
    }

    #[must_use]
    pub const fn jitter_target_ms(&self) -> u16 {
        self.jitter_target_ms
    }

    /// Expected decoded samples per channel for this configured frame duration.
    #[must_use]
    pub const fn expected_samples_per_frame(&self) -> u32 {
        (OPUS_SAMPLE_RATE / 1000) * (self.frame_duration_ms as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_constants_are_consistent() {
        assert_eq!(OPUS_SAMPLE_RATE, 48_000);
        assert_eq!(NOMINAL_PACKET_DURATION_MS, 10);
        assert_eq!(NOMINAL_SAMPLES_PER_FRAME, 480);
        assert_eq!(
            (OPUS_SAMPLE_RATE / 1000) * u32::from(NOMINAL_PACKET_DURATION_MS),
            u32::from(NOMINAL_SAMPLES_PER_FRAME)
        );
        assert_eq!(MAX_DECODED_SAMPLES, (48_000 / 1000) * 120);
    }

    #[test]
    fn direction_round_trip() {
        assert_eq!(AudioDirection::from_u8(0), Some(AudioDirection::Downlink));
        assert_eq!(AudioDirection::from_u8(1), Some(AudioDirection::Uplink));
        assert_eq!(AudioDirection::from_u8(2), None);
    }

    #[test]
    fn channels_count_and_round_trip() {
        assert_eq!(AudioChannels::Mono.count(), 1);
        assert_eq!(AudioChannels::Stereo.count(), 2);
        assert_eq!(AudioChannels::from_u8(1), Some(AudioChannels::Mono));
        assert_eq!(AudioChannels::from_u8(2), Some(AudioChannels::Stereo));
        assert_eq!(AudioChannels::from_u8(3), None);
    }

    #[test]
    fn stop_reason_round_trip() {
        for (code, expected) in [
            (1, AudioStopReason::UserMute),
            (2, AudioStopReason::SessionEnded),
            (3, AudioStopReason::DeviceChanged),
            (4, AudioStopReason::BufferOverflow),
            (5, AudioStopReason::HostDisabled),
        ] {
            assert_eq!(AudioStopReason::from_u8(code), Some(expected));
        }
        assert_eq!(AudioStopReason::from_u8(0), None);
        assert_eq!(AudioStopReason::from_u8(6), None);
    }

    #[test]
    fn config_validation_enforces_bounds() {
        let generation = AudioGeneration::INITIAL;
        let valid = AudioStreamConfig::new(
            AudioDirection::Downlink,
            generation,
            AudioChannels::Stereo,
            10,
            20,
        )
        .unwrap();
        assert_eq!(valid.expected_samples_per_frame(), 480);
        assert_eq!(valid.sample_rate(), 48_000);

        // Zero duration rejected
        assert_eq!(
            AudioStreamConfig::new(AudioDirection::Downlink, generation, AudioChannels::Stereo, 0, 20),
            Err(AudioConfigError::InvalidDuration)
        );

        // Excessive duration rejected
        assert_eq!(
            AudioStreamConfig::new(AudioDirection::Downlink, generation, AudioChannels::Stereo, 121, 20),
            Err(AudioConfigError::InvalidDuration)
        );

        // Zero jitter target rejected
        assert_eq!(
            AudioStreamConfig::new(AudioDirection::Downlink, generation, AudioChannels::Stereo, 10, 0),
            Err(AudioConfigError::InvalidJitterTarget)
        );

        // Jitter exceeding ceiling rejected
        assert_eq!(
            AudioStreamConfig::new(AudioDirection::Downlink, generation, AudioChannels::Stereo, 10, 101),
            Err(AudioConfigError::InvalidJitterTarget)
        );
    }
}
