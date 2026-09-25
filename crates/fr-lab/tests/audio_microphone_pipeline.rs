#![forbid(unsafe_code)]
//! Deterministic lab tests (not end-to-end or qualification evidence) for the
//! client-to-host microphone pipeline, over synthetic codec and virtual-mic stand-ins.
//!
//! Conforms to plan section 15.4 and bead `fr-p2-audio-microphone-lz6`:
//! 1. Hot-mic security test: activation without explicit enable must fail; connection alone never enables capture.
//! 2. Push-to-talk state machine: transmits only while actively triggered.
//! 3. Lease expiry and approval revocation: uplink silenced at host boundary immediately with zero residual buffered playback.
//! 4. Endpoint absence produces typed capability refusal: no fake device, no silent drop.
//! 5. Audio generation fencing: stale buffered speech dropped across reconnects and device switches.
//! 6. Wire-level packet framing and decoding in memory (no network).

use fr_client::audio::{ClientMicController, MicControllerError};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MicEndpointStatus,
    MicPermission, MicTalkMode, NOMINAL_PACKET_DURATION_MS,
};
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioPcmFrame, SyntheticAudioDecoder};
use fr_media::virtual_mic::{
    HostAudioUplinkPipeline, LinuxPipeWireMicEndpoint, MacOSCoreAudioMicEndpoint, MicEndpointError,
    SyntheticVirtualMicEndpoint, UplinkPipelineError, VirtualMicEndpoint,
    WindowsVirtualAudioMicEndpoint,
};
use fr_wire::audio::{AudioPacket, decode_packet, encode_packet};

fn make_synthetic_packet(
    direction: AudioDirection,
    generation: AudioGeneration,
    seq: u64,
    ts: u64,
) -> AudioAccessUnit {
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(&seq.to_le_bytes());
    payload[8..10].copy_from_slice(&480u16.to_le_bytes());
    payload[10..12].copy_from_slice(&1000i16.to_le_bytes());
    payload[12] = 0xAA;
    payload[13] = 0x55;
    payload[14] = 1; // mono
    payload[15] = 0x01;
    AudioAccessUnit::new(direction, generation, seq, ts, 480, false, &payload).unwrap()
}

#[test]
fn hot_mic_security_test_activation_requires_explicit_enable() {
    let generation = AudioGeneration::INITIAL;
    let mut ctrl = ClientMicController::new(generation, AudioChannels::Mono).unwrap();

    // Invariant 1: Must NOT be enabled or transmitting upon creation/connect
    assert!(!ctrl.is_explicitly_enabled());
    assert!(!ctrl.is_transmitting());
    assert_eq!(ctrl.talk_mode(), MicTalkMode::Muted);
    assert_eq!(ctrl.permission(), MicPermission::NotRequested);

    // Invariant 2: Audio frames submitted are discarded without transmission
    let samples = vec![2500i16; 480];
    let frame =
        AudioPcmFrame::from_interleaved(generation, AudioChannels::Mono, 0, &samples).unwrap();
    assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());
    assert_eq!(ctrl.total_packets_transmitted(), 0);

    // Invariant 3: Explicit enable fails without OS microphone permission
    let err = ctrl.set_explicit_enabled(true).unwrap_err();
    assert_eq!(err, MicControllerError::PermissionDenied);
    assert!(!ctrl.is_explicitly_enabled());

    // Invariant 4: Granting OS permission still leaves mic inactive until explicitly enabled
    ctrl.set_permission(MicPermission::Granted);
    assert!(!ctrl.is_explicitly_enabled());
    assert!(!ctrl.is_transmitting());
    assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());

    // Invariant 5: User explicitly toggles microphone on in client UI
    ctrl.set_explicit_enabled(true).unwrap();
    assert!(ctrl.is_explicitly_enabled());
    // Still muted by default until Push-To-Talk or OpenMic is activated
    assert!(!ctrl.is_transmitting());
    assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());

    // Invariant 6: Push-To-Talk activation triggers transmission
    ctrl.set_talk_mode(MicTalkMode::PushToTalk { active: true });
    assert!(ctrl.is_transmitting());
    let unit = ctrl
        .process_captured_pcm(&frame)
        .unwrap()
        .expect("packet emitted");
    assert_eq!(unit.sequence(), 0);
    assert_eq!(unit.generation(), generation);
    assert_eq!(ctrl.total_packets_transmitted(), 1);
    assert!(ctrl.current_rms() > 0.0);

    // Invariant 7: Push-To-Talk release ceases transmission instantly
    ctrl.set_talk_mode(MicTalkMode::PushToTalk { active: false });
    assert!(!ctrl.is_transmitting());
    assert!(ctrl.process_captured_pcm(&frame).unwrap().is_none());
    assert_eq!(ctrl.total_packets_transmitted(), 1);

    // Invariant 8: Explicit disable immediately resets talk mode and transmission
    ctrl.set_talk_mode(MicTalkMode::OpenMic { active: true });
    assert!(ctrl.is_transmitting());
    ctrl.set_explicit_enabled(false).unwrap();
    assert!(!ctrl.is_explicitly_enabled());
    assert!(!ctrl.is_transmitting());
    assert_eq!(ctrl.talk_mode(), MicTalkMode::Muted);
}

#[test]
fn lease_expiry_and_revocation_immediately_silences_uplink() {
    let generation = AudioGeneration::INITIAL;
    let config = AudioStreamConfig::new(
        AudioDirection::Uplink,
        generation,
        AudioChannels::Mono,
        NOMINAL_PACKET_DURATION_MS,
        20,
    )
    .unwrap();

    let mut decoder = SyntheticAudioDecoder::new();
    decoder.configure(config).unwrap();

    let endpoint = SyntheticVirtualMicEndpoint::new_qualified("test-mic");
    let mut pipeline =
        HostAudioUplinkPipeline::new(config, Box::new(decoder), Box::new(endpoint)).unwrap();

    // 1. Authorized delivery
    let unit = make_synthetic_packet(AudioDirection::Uplink, generation, 0, 0);
    pipeline.process_access_unit(&unit, true, true).unwrap();
    assert_eq!(pipeline.injected_frame_count(), 1);

    // 2. Lease expired at OS submission time -> immediately silenced and rejected
    let unit2 = make_synthetic_packet(AudioDirection::Uplink, generation, 1, 480);
    let res = pipeline.process_access_unit(&unit2, false, true);
    assert_eq!(res, Err(UplinkPipelineError::AuthorityRevoked));

    // 3. Approval revoked -> immediately silenced and rejected
    let unit3 = make_synthetic_packet(AudioDirection::Uplink, generation, 2, 960);
    let res2 = pipeline.process_access_unit(&unit3, true, false);
    assert_eq!(res2, Err(UplinkPipelineError::AuthorityRevoked));

    // 4. Packet Loss Concealment (PLC) during lease lapse is also refused
    let plc_res = pipeline.process_loss_concealment(480, false, true);
    assert_eq!(plc_res, Err(UplinkPipelineError::AuthorityRevoked));
}

#[test]
fn endpoint_absence_produces_typed_capability_refusal() {
    // Linux missing PipeWire produces typed refusal
    let mut linux_missing = LinuxPipeWireMicEndpoint::with_status(MicEndpointStatus::Unsupported {
        os: "linux",
        reason: "PipeWire daemon not running",
    });
    assert!(!linux_missing.is_qualified());
    let samples = vec![0i16; 480];
    let frame =
        AudioPcmFrame::from_interleaved(AudioGeneration::INITIAL, AudioChannels::Mono, 0, &samples)
            .unwrap();
    let err = linux_missing.submit_pcm(&frame).unwrap_err();
    assert_eq!(
        err,
        MicEndpointError::EndpointNotQualified {
            os: "linux",
            reason: "PipeWire daemon not running",
        }
    );

    // macOS missing driver produces typed refusal
    let mut macos_missing =
        MacOSCoreAudioMicEndpoint::with_status(MicEndpointStatus::Unsupported {
            os: "macos",
            reason: "signed CoreAudio server plugin not found",
        });
    assert!(!macos_missing.is_qualified());
    let err_mac = macos_missing.submit_pcm(&frame).unwrap_err();
    assert_eq!(
        err_mac,
        MicEndpointError::EndpointNotQualified {
            os: "macos",
            reason: "signed CoreAudio server plugin not found",
        }
    );

    // Windows missing driver produces typed refusal
    let mut windows_missing =
        WindowsVirtualAudioMicEndpoint::with_status(MicEndpointStatus::Unsupported {
            os: "windows",
            reason: "virtual audio endpoint driver not installed",
        });
    assert!(!windows_missing.is_qualified());
    let err_win = windows_missing.submit_pcm(&frame).unwrap_err();
    assert_eq!(
        err_win,
        MicEndpointError::EndpointNotQualified {
            os: "windows",
            reason: "virtual audio endpoint driver not installed",
        }
    );
}

#[test]
fn generation_fencing_prevents_stale_buffered_speech() {
    let gen_1 = AudioGeneration::INITIAL;
    let gen_2 = gen_1.next().expect("valid next generation");

    let config = AudioStreamConfig::new(
        AudioDirection::Uplink,
        gen_1,
        AudioChannels::Mono,
        NOMINAL_PACKET_DURATION_MS,
        20,
    )
    .unwrap();

    let mut decoder = SyntheticAudioDecoder::new();
    decoder.configure(config).unwrap();

    let endpoint = SyntheticVirtualMicEndpoint::new_qualified("test-mic");
    let mut pipeline =
        HostAudioUplinkPipeline::new(config, Box::new(decoder), Box::new(endpoint)).unwrap();

    // Valid unit under gen_1
    let unit1 = make_synthetic_packet(AudioDirection::Uplink, gen_1, 0, 0);
    pipeline.process_access_unit(&unit1, true, true).unwrap();
    assert_eq!(pipeline.injected_frame_count(), 1);

    // Device switch / reconnect advances generation to gen_2
    pipeline.handle_generation_change(gen_2);

    // Stale unit from gen_1 arriving late is rejected by generation fence
    let stale_unit = make_synthetic_packet(AudioDirection::Uplink, gen_1, 1, 480);
    let err = pipeline
        .process_access_unit(&stale_unit, true, true)
        .unwrap_err();
    assert_eq!(
        err,
        UplinkPipelineError::GenerationMismatch {
            expected: gen_2,
            actual: gen_1,
        }
    );

    // Valid unit under new gen_2 succeeds
    let fresh_unit = make_synthetic_packet(AudioDirection::Uplink, gen_2, 2, 480);
    pipeline
        .process_access_unit(&fresh_unit, true, true)
        .unwrap();
    assert_eq!(pipeline.injected_frame_count(), 2);
}

#[test]
fn lab_client_to_host_microphone_wire_pipeline() {
    let generation = AudioGeneration::INITIAL;
    let binding = 0xABCD_1234;

    // 1. Client side setup
    let mut client_mic = ClientMicController::new(generation, AudioChannels::Mono).unwrap();
    client_mic.set_permission(MicPermission::Granted);
    client_mic.set_explicit_enabled(true).unwrap();
    client_mic.set_talk_mode(MicTalkMode::PushToTalk { active: true });

    // 2. Host side setup
    let config = AudioStreamConfig::new(
        AudioDirection::Uplink,
        generation,
        AudioChannels::Mono,
        NOMINAL_PACKET_DURATION_MS,
        20,
    )
    .unwrap();
    let mut host_decoder = SyntheticAudioDecoder::new();
    host_decoder.configure(config).unwrap();
    let host_endpoint = SyntheticVirtualMicEndpoint::new_qualified("fr-virtual-mic");
    let mut host_pipeline =
        HostAudioUplinkPipeline::new(config, Box::new(host_decoder), Box::new(host_endpoint))
            .unwrap();

    // 3. Capture 5 frames of speech (50 ms total)
    for i in 0..5 {
        let sample_val = i16::try_from((i + 1) * 1000).unwrap();
        let pcm_samples = vec![sample_val; 480];
        let pcm_in =
            AudioPcmFrame::from_interleaved(generation, AudioChannels::Mono, i * 480, &pcm_samples)
                .unwrap();

        // Client processes & encodes
        let access_unit = client_mic
            .process_captured_pcm(&pcm_in)
            .unwrap()
            .expect("packet generated");

        // Convert to wire AudioPacket
        let wire_packet = AudioPacket {
            direction: AudioDirection::Uplink,
            generation: access_unit.generation(),
            sequence: access_unit.sequence(),
            timestamp_samples: access_unit.timestamp_samples(),
            duration_samples: access_unit.duration_samples(),
            payload: access_unit.payload(),
        };

        // Wire serialize
        let mut wire_buffer = [0u8; 1500];
        let encoded_len = encode_packet(&wire_packet, binding, &mut wire_buffer).unwrap();

        // Wire deserialize
        let decoded_packet = decode_packet(&wire_buffer[..encoded_len], binding).unwrap();
        assert_eq!(decoded_packet.direction, AudioDirection::Uplink);
        assert_eq!(decoded_packet.sequence, i);
        assert_eq!(decoded_packet.generation, generation);

        // Convert back to AudioAccessUnit
        let received_unit = AudioAccessUnit::new(
            decoded_packet.direction,
            decoded_packet.generation,
            decoded_packet.sequence,
            decoded_packet.timestamp_samples,
            decoded_packet.duration_samples,
            false,
            decoded_packet.payload,
        )
        .unwrap();

        // Host pipeline receives and injects into virtual mic endpoint
        host_pipeline
            .process_access_unit(&received_unit, true, true)
            .unwrap();
    }

    assert_eq!(client_mic.total_packets_transmitted(), 5);
    assert_eq!(host_pipeline.received_packet_count(), 5);
    assert_eq!(host_pipeline.injected_frame_count(), 5);
    assert!(host_pipeline.endpoint().is_qualified());
}
