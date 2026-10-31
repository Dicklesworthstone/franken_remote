//! Consent belongs to one audio generation; reconnects cannot revive it.
use super::*;
use fr_media::audio::{AudioMediaError, SyntheticAudioEncoder};
use std::{cell::RefCell, rc::Rc};

fn controller() -> ClientMicController {
    ClientMicController::with_encoder(
        AudioGeneration::from_raw(7),
        AudioChannels::Mono,
        Box::new(SyntheticAudioEncoder::new()),
    )
    .unwrap()
}

fn enable(controller: &mut ClientMicController) {
    controller.set_permission(MicPermission::Granted);
    controller.set_explicit_enabled(true).unwrap();
    controller.set_talk_mode(MicTalkMode::OpenMic { active: true });
    assert!(controller.is_transmitting());
}

#[test]
fn every_loss_of_permission_requires_a_new_explicit_enable() {
    for permission in [
        MicPermission::NotRequested,
        MicPermission::Denied,
        MicPermission::Restricted,
    ] {
        let mut controller = controller();
        enable(&mut controller);
        controller.set_permission(permission);
        controller.set_permission(MicPermission::Granted);
        assert!(!controller.is_explicitly_enabled(), "{permission:?}");
        assert!(!controller.is_transmitting(), "{permission:?}");
        assert_eq!(controller.talk_mode(), MicTalkMode::Muted);
    }
}

#[test]
fn a_new_generation_does_not_inherit_permission_or_open_mic_intent() {
    let mut controller = controller();
    enable(&mut controller);
    controller
        .reset_generation(AudioGeneration::from_raw(8))
        .unwrap();
    assert!(!controller.is_explicitly_enabled());
    assert!(!controller.is_transmitting());
    assert_eq!(controller.permission(), MicPermission::NotRequested);
    assert_eq!(controller.talk_mode(), MicTalkMode::Muted);
}

fn pcm(generation: u64, channels: AudioChannels, at: u64, samples: usize) -> AudioPcmFrame {
    AudioPcmFrame::from_interleaved(
        AudioGeneration::from_raw(generation),
        channels,
        at,
        &vec![5_000; samples],
    )
    .unwrap()
}

#[test]
fn an_inactive_initial_push_to_talk_mode_preserves_explicit_consent() {
    let mut controller = controller();
    controller.set_permission(MicPermission::Granted);
    controller.set_explicit_enabled(true).unwrap();
    controller.set_talk_mode(MicTalkMode::PushToTalk { active: false });
    assert!(controller.is_explicitly_enabled());
    assert!(!controller.is_transmitting());
    controller.set_talk_mode(MicTalkMode::PushToTalk { active: true });
    assert!(controller.is_transmitting());
}

#[test]
fn stopping_a_used_talk_epoch_requires_a_fresh_configuration_and_enable() {
    for stop in 0..3 {
        let mut controller = controller();
        enable(&mut controller);
        assert!(
            controller
                .process_captured_pcm(&pcm(7, AudioChannels::Mono, 0, 480))
                .unwrap()
                .is_some()
        );
        match stop {
            0 => controller.set_explicit_enabled(false).unwrap(),
            1 => controller.set_talk_mode(MicTalkMode::PushToTalk { active: false }),
            _ => controller.set_permission(MicPermission::NotRequested),
        }
        controller.set_permission(MicPermission::Granted);
        assert_eq!(
            controller.set_explicit_enabled(true),
            Err(MicControllerError::GenerationRetired)
        );
        assert!(
            controller
                .process_captured_pcm(&pcm(7, AudioChannels::Mono, 480, 480))
                .unwrap()
                .is_none()
        );
        controller
            .reset_generation(AudioGeneration::from_raw(8))
            .unwrap();
        assert_eq!(
            controller.set_explicit_enabled(true),
            Err(MicControllerError::PermissionDenied)
        );
        enable(&mut controller);
        let unit = controller
            .process_captured_pcm(&pcm(8, AudioChannels::Mono, 0, 480))
            .unwrap()
            .unwrap();
        assert_eq!(unit.generation(), AudioGeneration::from_raw(8));
        assert_eq!(unit.sequence(), 0);
        assert_eq!(controller.total_packets_transmitted(), 2);
    }
}

#[derive(Default)]
struct Calls {
    submitted: usize,
    polled: usize,
    packet: Option<AudioAccessUnit>,
}

/// An opaque backend may retain old output or refuse reconfiguration. The
/// controller must never turn that into renewed microphone consent.
struct OpaqueEncoder {
    calls: Rc<RefCell<Calls>>,
    config: Option<AudioStreamConfig>,
    fail_generation: Option<u64>,
}
impl AudioEncoder for OpaqueEncoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        if self.fail_generation == Some(config.generation().as_raw()) {
            return Err(AudioMediaError::Fatal);
        }
        self.config = Some(config);
        Ok(())
    }
    fn submit_pcm(&mut self, _: &AudioPcmFrame) -> Result<(), AudioMediaError> {
        self.calls.borrow_mut().submitted += 1;
        Ok(())
    }
    fn poll_packet(&mut self) -> Result<Option<AudioAccessUnit>, AudioMediaError> {
        let mut calls = self.calls.borrow_mut();
        calls.polled += 1;
        Ok(calls.packet.take())
    }
    fn configuration(&self) -> Option<AudioStreamConfig> {
        self.config
    }
    fn reset(&mut self) {
        // Deliberately cannot prove cancellation of this backend's old reply.
    }
}

fn opaque(
    packet: Option<&AudioAccessUnit>,
    fail_generation: Option<u64>,
) -> (ClientMicController, Rc<RefCell<Calls>>) {
    let calls = Rc::new(RefCell::new(Calls {
        packet: packet.copied(),
        ..Calls::default()
    }));
    let controller = ClientMicController::with_encoder(
        AudioGeneration::from_raw(7),
        AudioChannels::Mono,
        Box::new(OpaqueEncoder {
            calls: calls.clone(),
            config: None,
            fail_generation,
        }),
    )
    .unwrap();
    (controller, calls)
}

fn packet(
    direction: AudioDirection,
    generation: u64,
    sequence: u64,
    at: u64,
    duration: u16,
) -> AudioAccessUnit {
    AudioAccessUnit::new(
        direction,
        AudioGeneration::from_raw(generation),
        sequence,
        at,
        duration,
        false,
        &[1],
    )
    .unwrap()
}

#[test]
fn failed_reconfiguration_is_visible_and_cannot_reuse_the_failed_generation() {
    let (mut controller, _) = opaque(None, Some(8));
    enable(&mut controller);
    let expected = MicControllerError::ConfigError(AudioMediaError::Fatal.to_string());
    assert_eq!(
        controller.reset_generation(AudioGeneration::from_raw(8)),
        Err(expected.clone())
    );
    assert_eq!(controller.error(), Some(&expected));
    assert!(!controller.is_transmitting());
    controller.set_permission(MicPermission::Granted);
    assert_eq!(controller.set_explicit_enabled(true), Err(expected));
    assert_eq!(
        controller.reset_generation(AudioGeneration::from_raw(8)),
        Err(MicControllerError::GenerationNotAdvanced)
    );
    assert_eq!(
        controller.reset_generation(AudioGeneration::from_raw(7)),
        Err(MicControllerError::GenerationNotAdvanced)
    );
    controller
        .reset_generation(AudioGeneration::from_raw(9))
        .unwrap();
    assert!(controller.error().is_none());
    assert!(!controller.is_explicitly_enabled());
    assert_eq!(controller.permission(), MicPermission::NotRequested);
}

#[test]
fn old_queued_sound_cannot_escape_a_new_generation_even_if_reset_did_not_erase_it() {
    let (mut controller, calls) = opaque(Some(&packet(AudioDirection::Uplink, 7, 0, 0, 480)), None);
    controller
        .reset_generation(AudioGeneration::from_raw(8))
        .unwrap();
    enable(&mut controller);
    assert_eq!(
        controller.process_captured_pcm(&pcm(8, AudioChannels::Mono, 0, 480)),
        Err(MicControllerError::InvalidPacket)
    );
    assert_eq!(calls.borrow().submitted, 1);
    assert_eq!(controller.total_packets_transmitted(), 0);
    assert!(!controller.is_explicitly_enabled());
}

#[test]
fn mismatched_encoded_direction_sequence_position_and_duration_never_leave_the_controller() {
    for wrong in [
        packet(AudioDirection::Downlink, 7, 0, 0, 480),
        packet(AudioDirection::Uplink, 7, 1, 0, 480),
        packet(AudioDirection::Uplink, 7, 0, 480, 480),
        packet(AudioDirection::Uplink, 7, 0, 0, 960),
    ] {
        let (mut controller, _) = opaque(Some(&wrong), None);
        enable(&mut controller);
        assert_eq!(
            controller.process_captured_pcm(&pcm(7, AudioChannels::Mono, 0, 480)),
            Err(MicControllerError::InvalidPacket)
        );
        assert_eq!(controller.total_packets_transmitted(), 0);
        assert_eq!(controller.current_rms(), 0.0);
        assert!(!controller.is_transmitting());
    }
}

#[test]
fn deferred_output_does_not_accumulate_capture_or_resume_after_a_late_reply() {
    let (mut controller, calls) = opaque(None, None);
    enable(&mut controller);
    assert_eq!(
        controller.process_captured_pcm(&pcm(7, AudioChannels::Mono, 0, 480)),
        Err(MicControllerError::EncoderOutputPending)
    );
    calls.borrow_mut().packet = Some(packet(AudioDirection::Uplink, 7, 0, 0, 480));
    for at in [480, 960, 1_440] {
        assert!(
            controller
                .process_captured_pcm(&pcm(7, AudioChannels::Mono, at, 480))
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(calls.borrow().submitted, 1);
    assert_eq!(calls.borrow().polled, 1);
    assert_eq!(controller.total_packets_transmitted(), 0);
    assert_eq!(
        controller.set_explicit_enabled(true),
        Err(MicControllerError::EncoderOutputPending)
    );
}

#[test]
fn stale_or_malformed_capture_is_refused_before_the_encoder_receives_it() {
    for (generation, channels, samples) in [
        (6, AudioChannels::Mono, 480),
        (8, AudioChannels::Mono, 480),
        (7, AudioChannels::Stereo, 960),
        (7, AudioChannels::Mono, 240),
    ] {
        let frame = pcm(generation, channels, 0, samples);
        let (mut controller, calls) = opaque(None, None);
        enable(&mut controller);
        assert_eq!(
            controller.process_captured_pcm(&frame),
            Err(MicControllerError::InvalidFrame)
        );
        assert_eq!(calls.borrow().submitted, 0);
        assert!(!controller.is_transmitting());
    }
}

#[test]
fn overlapping_capture_and_exhausted_sample_positions_fail_closed() {
    let mut controller = controller();
    enable(&mut controller);
    controller
        .process_captured_pcm(&pcm(7, AudioChannels::Mono, 480, 480))
        .unwrap();
    assert_eq!(
        controller.process_captured_pcm(&pcm(7, AudioChannels::Mono, 480, 480)),
        Err(MicControllerError::InvalidFrame)
    );
    let (mut controller, calls) = opaque(None, None);
    enable(&mut controller);
    assert_eq!(
        controller.process_captured_pcm(&pcm(7, AudioChannels::Mono, u64::MAX, 480)),
        Err(MicControllerError::CounterOverflow)
    );
    assert_eq!(calls.borrow().submitted, 0);
}
