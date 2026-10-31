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
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame};

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
    /// A retired audio epoch cannot carry another talk segment.
    GenerationRetired,
    /// Reconfiguration must advance past every previously attempted generation.
    GenerationNotAdvanced,
    /// Captured PCM does not match the original format or sample timeline.
    InvalidFrame,
    /// Encoder output does not belong to the exact submitted frame/epoch.
    InvalidPacket,
    /// This synchronous capture boundary cannot retain deferred encoder work.
    EncoderOutputPending,
    /// A sequence, sample position, or emitted-packet counter was exhausted.
    CounterOverflow,
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
            Self::GenerationRetired => {
                write!(f, "microphone talk requires a fresh audio generation")
            }
            Self::GenerationNotAdvanced => write!(f, "microphone generation did not advance"),
            Self::InvalidFrame => write!(f, "microphone capture format or timeline mismatch"),
            Self::InvalidPacket => write!(f, "microphone encoder output mismatch"),
            Self::EncoderOutputPending => {
                write!(f, "microphone encoder did not complete this frame")
            }
            Self::CounterOverflow => write!(f, "microphone audio counter exhausted"),
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
    /// Current talk mode (`Muted`, `PushToTalk`, or `OpenMic`).
    talk_mode: MicTalkMode,
    /// Active audio generation (fences reconnects and device changes).
    generation: AudioGeneration,
    /// Stream configuration.
    config: AudioStreamConfig,
    /// Opus audio encoder.
    encoder: Box<dyn AudioEncoder>,
    /// Monotonic sequence counter for uplink packets.
    sequence: u64,
    /// Each admitted source interval is consumed, even on encoder failure.
    last_input_end: Option<u64>,
    /// A fault is terminal for this generation; consent alone cannot clear it.
    fault: Option<MicControllerError>,
    /// Last transmitted audio level (RMS energy in 0.0..=1.0 range).
    current_rms: f32,
    /// Total packets successfully transmitted.
    total_packets_transmitted: u64,
}

impl ClientMicController {
    /// Create a client microphone controller around an explicitly supplied Opus
    /// encoder (in production, fr-native's libopus encoder).
    ///
    /// There is deliberately no default encoder: a controller that silently fell
    /// back to a placeholder would emit non-Opus bytes as microphone audio. The
    /// encoder is configured for the uplink stream here; a refusal is returned as
    /// [`MicControllerError::ConfigError`] and no controller is built.
    ///
    /// By invariant, `explicit_enabled` is initialized to `false` and talk mode to `Muted`.
    pub fn with_encoder(
        generation: AudioGeneration,
        channels: AudioChannels,
        mut encoder: Box<dyn AudioEncoder>,
    ) -> Result<Self, MicControllerError> {
        let config = AudioStreamConfig::new(
            AudioDirection::Uplink,
            generation,
            channels,
            NOMINAL_PACKET_DURATION_MS,
            20,
        )
        .map_err(|e| MicControllerError::ConfigError(e.to_string()))?;
        encoder
            .configure(config)
            .map_err(|e| MicControllerError::ConfigError(e.to_string()))?;
        if encoder.configuration() != Some(config) {
            return Err(MicControllerError::InvalidPacket);
        }

        Ok(Self {
            explicit_enabled: false,
            permission: MicPermission::NotRequested,
            talk_mode: MicTalkMode::Muted,
            generation,
            config,
            encoder,
            sequence: 0,
            last_input_end: None,
            fault: None,
            current_rms: 0.0,
            total_packets_transmitted: 0,
        })
    }

    /// Set operating system microphone permission state.
    pub fn set_permission(&mut self, permission: MicPermission) {
        self.permission = permission;
        if permission != MicPermission::Granted {
            // Unknown permission is loss of authority too. A later OS grant
            // must not restore a previous open-mic or held push-to-talk intent.
            self.fence();
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
            if let Some(error) = &self.fault {
                return Err(error.clone());
            }
            if self.permission != MicPermission::Granted {
                return Err(MicControllerError::PermissionDenied);
            }
            self.explicit_enabled = true;
        } else {
            self.fence();
        }
        Ok(())
    }

    /// Returns whether the user has explicitly enabled microphone forwarding.
    #[must_use]
    pub fn is_explicitly_enabled(&self) -> bool {
        self.explicit_enabled
    }

    /// Update talk mode (e.g. key press for Push-to-Talk or Mute toggle).
    /// Stopping an epoch which admitted PCM is terminal; a later talk segment
    /// needs a fresh configuration and explicit enable, never a codec reset
    /// which might preserve queued speech or reuse old packet identifiers.
    pub fn set_talk_mode(&mut self, mode: MicTalkMode) {
        if !self.explicit_enabled {
            self.talk_mode = MicTalkMode::Muted;
            return;
        }
        self.talk_mode = mode;
        if !mode.is_active() {
            if self.last_input_end.is_some() {
                self.fence();
            } else {
                self.current_rms = 0.0;
            }
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
    /// 3. Current talk mode `is_active()` is true (PTT pressed or `OpenMic` active).
    #[must_use]
    pub fn is_transmitting(&self) -> bool {
        self.explicit_enabled
            && self.permission == MicPermission::Granted
            && self.talk_mode.is_active()
            && self.fault.is_none()
    }

    /// Terminal failure of the current audio epoch. A strictly newer,
    /// successfully configured epoch and fresh explicit consent are required.
    pub fn error(&self) -> Option<&MicControllerError> {
        self.fault.as_ref()
    }

    fn fence(&mut self) {
        self.explicit_enabled = false;
        self.talk_mode = MicTalkMode::Muted;
        self.current_rms = 0.0;
        if self.last_input_end.is_some() && self.fault.is_none() {
            self.fault = Some(MicControllerError::GenerationRetired);
        }
        // Trait reset is best-effort: some native encoders refuse same-epoch
        // configuration. Never treat it as proof that queued sound was erased
        // or permit another frame in an epoch which held captured audio.
        self.encoder.reset();
    }

    fn fail<T>(&mut self, error: MicControllerError) -> Result<T, MicControllerError> {
        self.fault = Some(error.clone());
        self.fence();
        Err(error)
    }

    /// Process a captured PCM audio frame from the local recording device.
    ///
    /// Hot-mic guarantee: if `is_transmitting()` is false, returns `Ok(None)` immediately,
    /// dropping the audio without passing it to the encoder or wire.
    /// The supplied encoder must complete one packet synchronously; deferred
    /// work is refused so a later capture cannot release an older talk segment.
    /// Run this on the supervised capture worker, never the authority thread.
    /// The caller still owns capture-age, transport and current lease checks.
    pub fn process_captured_pcm(
        &mut self,
        frame: &AudioPcmFrame,
    ) -> Result<Option<AudioAccessUnit>, MicControllerError> {
        // Enforce strict hot-mic protection
        if !self.is_transmitting() {
            self.current_rms = 0.0;
            return Ok(None);
        }

        if frame.generation() != self.generation
            || frame.channels() != self.config.channels()
            || frame.sample_rate() != self.config.sample_rate()
            || u32::try_from(frame.samples_per_channel()).ok()
                != Some(self.config.expected_samples_per_frame())
            || self
                .last_input_end
                .is_some_and(|end| frame.timestamp_samples() != end)
        {
            return self.fail(MicControllerError::InvalidFrame);
        }
        let Some(end) = frame
            .timestamp_samples()
            .checked_add(u64::from(self.config.expected_samples_per_frame()))
        else {
            return self.fail(MicControllerError::CounterOverflow);
        };
        let Some(next) = self.sequence.checked_add(1) else {
            return self.fail(MicControllerError::CounterOverflow);
        };
        let Some(total) = self.total_packets_transmitted.checked_add(1) else {
            return self.fail(MicControllerError::CounterOverflow);
        };
        self.last_input_end = Some(end);
        if let Err(error) = self.encoder.submit_pcm(frame) {
            return self.fail(MicControllerError::EncoderError(error.to_string()));
        }
        let unit = match self.encoder.poll_packet() {
            Ok(Some(unit)) => unit,
            Ok(None) => return self.fail(MicControllerError::EncoderOutputPending),
            Err(error) => return self.fail(MicControllerError::EncoderError(error.to_string())),
        };
        if unit.direction() != AudioDirection::Uplink
            || unit.generation() != self.generation
            || unit.sequence() != self.sequence
            || unit.timestamp_samples() != frame.timestamp_samples()
            || u32::from(unit.duration_samples()) != self.config.expected_samples_per_frame()
        {
            return self.fail(MicControllerError::InvalidPacket);
        }
        self.sequence = next;
        self.total_packets_transmitted = total;
        self.current_rms = frame.rms_energy();
        Ok(Some(unit))
    }

    /// Handle audio generation change (e.g. device switch or reconnect).
    /// Fence before reconfiguration, clear OS/talk consent, and consume a
    /// strictly newer generation even when the encoder fails. No swallowed
    /// configure error and no late reply can make the old epoch usable again.
    pub fn reset_generation(
        &mut self,
        new_generation: AudioGeneration,
    ) -> Result<(), MicControllerError> {
        self.fence();
        self.permission = MicPermission::NotRequested;
        if !new_generation.supersedes(self.generation) {
            return self.fail(MicControllerError::GenerationNotAdvanced);
        }
        self.generation = new_generation;
        let config = match AudioStreamConfig::new(
            AudioDirection::Uplink,
            new_generation,
            self.config.channels(),
            self.config.frame_duration_ms(),
            self.config.jitter_target_ms(),
        ) {
            Ok(config) => config,
            Err(error) => return self.fail(MicControllerError::ConfigError(error.to_string())),
        };
        self.config = config;
        self.sequence = 0;
        self.last_input_end = None;
        if let Err(error) = self.encoder.configure(config) {
            return self.fail(MicControllerError::ConfigError(error.to_string()));
        }
        if self.encoder.configuration() != Some(config) {
            return self.fail(MicControllerError::InvalidPacket);
        }
        self.fault = None;
        Ok(())
    }

    /// Current live RMS energy (0.0 to 1.0) for VU meter UI.
    #[must_use]
    pub fn current_rms(&self) -> f32 {
        self.current_rms
    }

    /// Total packets emitted to the caller in this controller's lifetime.
    /// This does not acknowledge network transmission or host audio submission.
    #[must_use]
    pub fn total_packets_transmitted(&self) -> u64 {
        self.total_packets_transmitted
    }
}

#[cfg(test)]
#[path = "mic/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use fr_media::audio::{AudioMediaError, SyntheticAudioEncoder};

    fn synthetic(
        generation: AudioGeneration,
        channels: AudioChannels,
    ) -> Result<ClientMicController, MicControllerError> {
        ClientMicController::with_encoder(
            generation,
            channels,
            Box::new(SyntheticAudioEncoder::new()),
        )
    }

    /// An encoder that refuses configuration, standing in for a missing or
    /// unqualified native Opus encoder.
    struct RefusingEncoder;

    impl AudioEncoder for RefusingEncoder {
        fn configure(&mut self, _config: AudioStreamConfig) -> Result<(), AudioMediaError> {
            Err(AudioMediaError::UnsupportedFormat)
        }

        fn submit_pcm(&mut self, _pcm: &AudioPcmFrame) -> Result<(), AudioMediaError> {
            Err(AudioMediaError::NotConfigured)
        }

        fn poll_packet(&mut self) -> Result<Option<AudioAccessUnit>, AudioMediaError> {
            Err(AudioMediaError::NotConfigured)
        }

        fn configuration(&self) -> Option<AudioStreamConfig> {
            None
        }
    }

    #[test]
    fn controller_refuses_an_encoder_that_cannot_be_configured() {
        let result = ClientMicController::with_encoder(
            AudioGeneration::INITIAL,
            AudioChannels::Mono,
            Box::new(RefusingEncoder),
        );
        let Err(MicControllerError::ConfigError(reason)) = result else {
            panic!("an unconfigurable encoder must be a typed refusal");
        };
        assert_eq!(reason, AudioMediaError::UnsupportedFormat.to_string());
    }

    #[test]
    fn supplied_encoder_is_configured_for_the_uplink_stream() {
        let generation = AudioGeneration::INITIAL;
        let ctrl = synthetic(generation, AudioChannels::Mono).unwrap();
        let configured = ctrl.encoder.configuration().expect("encoder configured");
        assert_eq!(configured.direction(), AudioDirection::Uplink);
        assert_eq!(configured.generation(), generation);
        assert_eq!(configured.channels(), AudioChannels::Mono);
    }

    #[test]
    fn test_hot_mic_protection_strictly_enforced() {
        let generation = AudioGeneration::INITIAL;
        let mut ctrl = synthetic(generation, AudioChannels::Mono).unwrap();

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
        let mut ctrl = synthetic(generation, AudioChannels::Mono).unwrap();
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
