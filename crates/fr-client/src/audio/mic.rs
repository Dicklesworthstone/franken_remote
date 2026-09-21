#![forbid(unsafe_code)]
//! Client microphone capture, talk mode state machine, and hot-mic protection.
//!
//! Per plan section 15.4 and PROTOCOL.md:
//! - Microphone forwarding is explicit-enable per session on the client — a talk
//!   toggle backed by the client OS's microphone permission — NEVER activated
//!   automatically by connecting, and surfaced by a visible indicator on both ends.
//! - Strict hot-mic protection: when disabled, muted, or when push-to-talk is inactive,
//!   no audio packets are ever encoded or transmitted across the wire.
//! - Generational resets ensure stale speech is never queued or replayed across reconnects.

use core::fmt;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MicPermission, MicTalkMode,
    NOMINAL_PACKET_DURATION_MS,
};
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame, SyntheticAudioEncoder};

/// Errors encountered in client microphone operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicControllerError {
    /// Microphone permission was denied by the user or OS.
    PermissionDenied,
    /// Explicit enable required: mic forwarding is never enabled automatically on connect.
    NotExplicitlyEnabled,
    /// Microphone is currently muted or inactive in push-to-talk mode.
    NotTransmitting,
    /// Encoder error occurred during compression.
    EncoderError(String),
    /// Configuration error.
    ConfigError(String),
}

impl fmt::Display for MicControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => write!(f, "microphone permission denied"),
            Self::NotExplicitlyEnabled => {
                write!(
                    f,
                    "microphone forwarding requires explicit per-session enable"
                )
            }
            Self::NotTransmitting => write!(f, "microphone is muted or push-to-talk is inactive"),
            Self::EncoderError(e) => write!(f, "microphone encoder error: {e}"),
            Self::ConfigError(e) => write!(f, "microphone config error: {e}"),
        }
    }
}

/// Client microphone capture controller and state machine.
pub struct ClientMicController {
    /// Whether the user has explicitly enabled microphone forwarding for this session.
    /// Invariant: MUST start `false` on every new connection. Never auto-enabled!
    explicit_enabled: bool,
    /// Operating system microphone permission status.
    permission: MicPermission,
    /// Current talk mode (Muted, PushToTalk, or OpenMic).
    talk_mode: MicTalkMode,
    /// Active audio generation (fences reconnects and device changes).
    generation: AudioGeneration,
    /// Stream configuration.
    config: AudioStreamConfig,
    /// Opus audio encoder.
    encoder: Box<dyn AudioEncoder>,
    /// Monotonic sequence counter for uplink packets.
    sequence: u64,
    /// Last transmitted audio level (RMS energy in 0.0..=1.0 range).
    current_rms: f32,
    /// Total packets successfully transmitted.
    total_packets_transmitted: u64,
}

impl ClientMicController {
    /// Create a new client microphone controller.
    ///
    /// By invariant, `explicit_enabled` is initialized to `false` and talk mode to `Muted`.
    pub fn new(
        generation: AudioGeneration,
        channels: AudioChannels,
    ) -> Result<Self, MicControllerError> {
        let config = AudioStreamConfig::new(
            AudioDirection::Uplink,
            generation,
            channels,
            NOMINAL_PACKET_DURATION_MS,
            20,
        )
        .map_err(|e| MicControllerError::ConfigError(e.to_string()))?;

        let mut encoder = Box::new(SyntheticAudioEncoder::new());
        encoder
            .configure(config)
            .map_err(|e| MicControllerError::ConfigError(e.to_string()))?;

        Ok(Self {
            explicit_enabled: false,
            permission: MicPermission::NotRequested,
            talk_mode: MicTalkMode::Muted,
            generation,
            config,
            encoder,
            sequence: 0,
            current_rms: 0.0,
            total_packets_transmitted: 0,
        })
    }

    /// Create a controller with a custom encoder (e.g. for testing or native Opus wrapper).
    pub fn with_encoder(
        generation: AudioGeneration,
        channels: AudioChannels,
        encoder: Box<dyn AudioEncoder>,
    ) -> Result<Self, MicControllerError> {
        let config = AudioStreamConfig::new(
            AudioDirection::Uplink,
            generation,
            channels,
            NOMINAL_PACKET_DURATION_MS,
            20,
        )
        .map_err(|e| MicControllerError::ConfigError(e.to_string()))?;

        Ok(Self {
            explicit_enabled: false,
            permission: MicPermission::NotRequested,
            talk_mode: MicTalkMode::Muted,
            generation,
            config,
            encoder,
            sequence: 0,
            current_rms: 0.0,
            total_packets_transmitted: 0,
        })
    }

    /// Set operating system microphone permission state.
    pub fn set_permission(&mut self, permission: MicPermission) {
        self.permission = permission;
        if matches!(
            permission,
            MicPermission::Denied | MicPermission::Restricted
        ) {
            // Immediately disable and mute if permission was denied or restricted
            self.explicit_enabled = false;
            self.talk_mode = MicTalkMode::Muted;
            self.encoder.reset();
            self.current_rms = 0.0;
        }
    }

    /// Get current OS permission state.
    #[must_use]
    pub fn permission(&self) -> MicPermission {
        self.permission
    }

    /// Explicitly enable or disable microphone forwarding for this session.
    ///
    /// Refuses with `MicControllerError::PermissionDenied` if OS permission is not granted.
    pub fn set_explicit_enabled(&mut self, enabled: bool) -> Result<(), MicControllerError> {
        if enabled {
            if self.permission != MicPermission::Granted {
                return Err(MicControllerError::PermissionDenied);
            }
            self.explicit_enabled = true;
        } else {
            self.explicit_enabled = false;
            self.talk_mode = MicTalkMode::Muted;
            self.encoder.reset();
            self.current_rms = 0.0;
        }
        Ok(())
    }

    /// Returns whether the user has explicitly enabled microphone forwarding.
    #[must_use]
    pub fn is_explicitly_enabled(&self) -> bool {
        self.explicit_enabled
    }

    /// Update talk mode (e.g. key press for Push-to-Talk or Mute toggle).
    pub fn set_talk_mode(&mut self, mode: MicTalkMode) {
        if !self.explicit_enabled {
            self.talk_mode = MicTalkMode::Muted;
            return;
        }
        self.talk_mode = mode;
        if !mode.is_active() {
            self.encoder.reset();
            self.current_rms = 0.0;
        }
    }

    /// Get current talk mode.
    #[must_use]
    pub fn talk_mode(&self) -> MicTalkMode {
        self.talk_mode
    }

    /// Query whether microphone is currently transmitting audio.
    ///
    /// Transmitting is true ONLY IF:
    /// 1. `explicit_enabled` is true.
    /// 2. OS permission is `Granted`.
    /// 3. Current talk mode `is_active()` is true (PTT pressed or OpenMic active).
    #[must_use]
    pub fn is_transmitting(&self) -> bool {
        self.explicit_enabled
            && self.permission == MicPermission::Granted
            && self.talk_mode.is_active()
    }

    /// Process a captured PCM audio frame from the local recording device.
    ///
    /// Hot-mic guarantee: if `is_transmitting()` is false, returns `Ok(None)` immediately,
    /// dropping the audio without passing it to the encoder or wire.
    pub fn process_captured_pcm(
        &mut self,
        frame: &AudioPcmFrame,
    ) -> Result<Option<AudioAccessUnit>, MicControllerError> {
        // Enforce strict hot-mic protection
        if !self.is_transmitting() {
            self.current_rms = 0.0;
            return Ok(None);
        }

        // Measure RMS energy for visual indicator
        self.current_rms = frame.rms_energy();

        // Submit PCM to encoder
        self.encoder
            .submit_pcm(frame)
            .map_err(|e| MicControllerError::EncoderError(e.to_string()))?;

        // Poll encoded access unit
        if let Some(unit) = self
            .encoder
            .poll_packet()
            .map_err(|e| MicControllerError::EncoderError(e.to_string()))?
        {
            self.sequence = self.sequence.saturating_add(1);
            self.total_packets_transmitted = self.total_packets_transmitted.saturating_add(1);
            Ok(Some(unit))
        } else {
            Ok(None)
        }
    }

    /// Handle audio generation change (e.g. device switch or reconnect).
    /// Drops any buffered encoder state and resets sequence/timestamps.
    pub fn reset_generation(&mut self, new_generation: AudioGeneration) {
        self.generation = new_generation;
        if let Ok(new_config) = AudioStreamConfig::new(
            AudioDirection::Uplink,
            new_generation,
            self.config.channels(),
            self.config.frame_duration_ms(),
            self.config.jitter_target_ms(),
        ) {
            self.config = new_config;
            let _ = self.encoder.configure(new_config);
        }
        self.encoder.reset();
        self.sequence = 0;
        self.current_rms = 0.0;
    }

    /// Current live RMS energy (0.0 to 1.0) for VU meter UI.
    #[must_use]
    pub fn current_rms(&self) -> f32 {
        self.current_rms
    }

    /// Total packets transmitted in this session.
    #[must_use]
    pub fn total_packets_transmitted(&self) -> u64 {
        self.total_packets_transmitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hot_mic_protection_strictly_enforced() {
        let generation = AudioGeneration::INITIAL;
        let mut ctrl = ClientMicController::new(generation, AudioChannels::Mono).unwrap();

        // Invariant: starts disabled and muted
        assert!(!ctrl.is_explicitly_enabled());
        assert!(!ctrl.is_transmitting());
        assert_eq!(ctrl.talk_mode(), MicTalkMode::Muted);
        assert_eq!(ctrl.permission(), MicPermission::NotRequested);

        // Captured PCM is dropped when not transmitting
        let samples = vec![5000i16; 480];
        let frame =
            AudioPcmFrame::from_interleaved(generation, AudioChannels::Mono, 0, &samples).unwrap();
        let out = ctrl.process_captured_pcm(&frame).unwrap();
        assert!(out.is_none());
        assert_eq!(ctrl.total_packets_transmitted(), 0);

        // Enabling fails if permission is not granted
        assert_eq!(
            ctrl.set_explicit_enabled(true),
            Err(MicControllerError::PermissionDenied)
        );

        // Grant permission
        ctrl.set_permission(MicPermission::Granted);
        ctrl.set_explicit_enabled(true).unwrap();
        assert!(ctrl.is_explicitly_enabled());

        // Still not transmitting until talk mode is active
        assert!(!ctrl.is_transmitting());
        assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());

        // Press Push-To-Talk
        ctrl.set_talk_mode(MicTalkMode::PushToTalk { active: true });
        assert!(ctrl.is_transmitting());

        // Now packet is encoded and emitted
        let unit = ctrl
            .process_captured_pcm(&frame)
            .unwrap()
            .expect("packet emitted");
        assert_eq!(unit.sequence(), 0);
        assert_eq!(unit.generation(), generation);
        assert_eq!(ctrl.total_packets_transmitted(), 1);
        assert!(ctrl.current_rms() > 0.0);

        // Release Push-To-Talk
        ctrl.set_talk_mode(MicTalkMode::PushToTalk { active: false });
        assert!(!ctrl.is_transmitting());
        assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());
        assert_eq!(ctrl.total_packets_transmitted(), 1);
    }

    #[test]
    fn test_revoking_permission_immediately_disables_mic() {
        let generation = AudioGeneration::INITIAL;
        let mut ctrl = ClientMicController::new(generation, AudioChannels::Mono).unwrap();
        ctrl.set_permission(MicPermission::Granted);
        ctrl.set_explicit_enabled(true).unwrap();
        ctrl.set_talk_mode(MicTalkMode::OpenMic { active: true });
        assert!(ctrl.is_transmitting());

        // OS revokes permission
        ctrl.set_permission(MicPermission::Denied);
        assert!(!ctrl.is_explicitly_enabled());
        assert!(!ctrl.is_transmitting());
        assert_eq!(ctrl.talk_mode(), MicTalkMode::Muted);
    }
}
