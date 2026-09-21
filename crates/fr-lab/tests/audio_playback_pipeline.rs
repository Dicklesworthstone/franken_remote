#![forbid(unsafe_code)]
//! Deterministic lab tests for host Opus audio downlink pipeline, jitter discipline,
//! packet loss concealment, generation resets, and bounded A/V offset.
//!
//! Conforms to plan sections 15.4, 23 Phase 2, and Bead fr-p2-audio-playback-lel.

use fr_client::audio::{
    AudioJitterBuffer, AudioVideoSyncController, AudioVolumeControl, AvAlignment, HostAudioStore,
    JitterDrainResult,
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig,
    WINDOWS_ENDPOINT_SCOPE_DISCLOSURE,
};
use fr_media::audio::{
    AudioAccessUnit, AudioDecoder, AudioEncoder, AudioPcmFrame, AudioResampler,
    SyntheticAudioDecoder, SyntheticAudioEncoder,
};
use fr_wire::audio::{AUDIO_PACKET_OVERHEAD, decode_packet, encode_packet};

#[test]
fn windows_loopback_scope_disclosure_verification() {
    assert!(
        !WINDOWS_ENDPOINT_SCOPE_DISCLOSURE.is_empty(),
        "disclosure string must not be empty"
    );
    assert!(
        WINDOWS_ENDPOINT_SCOPE_DISCLOSURE.contains("all terminal sessions")
            || WINDOWS_ENDPOINT_SCOPE_DISCLOSURE.contains("system-wide"),
        "disclosure must explicitly disclose cross-session / system-wide audio scope"
    );
    assert!(
        WINDOWS_ENDPOINT_SCOPE_DISCLOSURE.contains("not isolated"),
        "disclosure must explicitly clarify that capture is not isolated to the selected desktop user"
    );
}

#[test]
fn e2e_audio_pipeline_with_induced_loss_and_plc() {
    let generation = AudioGeneration::INITIAL;
    let config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        generation,
        AudioChannels::Stereo,
        10,
        20,
    )
    .expect("valid config");

    // 1. Host side: Capture and resample from 44.1 kHz source to 48 kHz standard
    let mut resampler = AudioResampler::new(44_100, 2, AudioChannels::Stereo).unwrap();
    let mut encoder = SyntheticAudioEncoder::new();
    encoder.configure(config).unwrap();

    let mut encoded_packets: Vec<AudioAccessUnit> = Vec::new();

    for frame_idx in 0..10 {
        // Generate 10 ms chunk of 44.1 kHz stereo audio (441 frames * 2 channels = 882 floats)
        let simulated_capture = vec![0.25f32; 441 * 2];
        let mut pcm_48k = AudioPcmFrame::empty(generation, AudioChannels::Stereo);
        resampler
            .resample_f32(
                generation,
                (frame_idx * 480) as u64,
                &simulated_capture,
                &mut pcm_48k,
            )
            .unwrap();

        encoder.submit_pcm(&pcm_48k).unwrap();
        if let Some(packet) = encoder.poll_packet().unwrap() {
            encoded_packets.push(packet);
        }
    }
    assert_eq!(encoded_packets.len(), 10);

    // 2. Wire serialization and simulated lossy network transport
    // Drop packet 2 and packet 5 (induced 20% loss); swap packets 3 and 4 (out-of-order)
    let mut wire_buffer = vec![0u8; AUDIO_PACKET_OVERHEAD + 128];
    let mut delivered_packets: Vec<AudioAccessUnit> = Vec::new();

    for (idx, packet) in encoded_packets.into_iter().enumerate() {
        if idx == 2 || idx == 5 {
            // Induced loss: simulate dropped packet over network
            continue;
        }

        // Wire encode
        let wire_packet = fr_wire::audio::AudioPacket {
            direction: packet.direction(),
            generation: packet.generation(),
            sequence: packet.sequence(),
            timestamp_samples: packet.timestamp_samples(),
            duration_samples: packet.duration_samples(),
            payload: packet.payload(),
        };

        let written = encode_packet(&wire_packet, 1, &mut wire_buffer).expect("wire encode");

        // Wire decode
        let decoded_wire = decode_packet(&wire_buffer[..written], 1).expect("wire decode");
        let client_unit = AudioAccessUnit::new(
            decoded_wire.direction,
            decoded_wire.generation,
            decoded_wire.sequence,
            decoded_wire.timestamp_samples,
            decoded_wire.duration_samples,
            false,
            decoded_wire.payload,
        )
        .expect("client access unit");

        delivered_packets.push(client_unit);
    }

    // Swap packets delivered at indices 2 and 3 (which correspond to seq 3 and seq 4)
    delivered_packets.swap(2, 3);

    // 3. Client side: Jitter buffer and decoder
    let mut jitter_buffer = AudioJitterBuffer::new(generation, 20);
    let mut decoder = SyntheticAudioDecoder::new();
    decoder.configure(config).unwrap();

    for pkt in delivered_packets {
        jitter_buffer.push_packet(pkt);
    }

    let mut played_count = 0;
    let mut plc_count = 0;

    // Drain all available frames from jitter buffer
    loop {
        match jitter_buffer.drain() {
            JitterDrainResult::Packet(pkt) => {
                decoder.submit_packet(&pkt).unwrap();
                let pcm = decoder.poll_pcm().unwrap().expect("pcm frame");
                assert_eq!(pcm.samples_per_channel(), 480);
                played_count += 1;
            }
            JitterDrainResult::Plc {
                duration_samples, ..
            } => {
                let pcm = decoder.decode_plc(duration_samples).unwrap();
                assert_eq!(pcm.samples_per_channel(), duration_samples as usize);
                plc_count += 1;
            }
            JitterDrainResult::Underrun => break,
        }
    }

    // Packets 2 and 5 were dropped, so exactly 2 PLC concealment events must be logged
    assert_eq!(plc_count, 2);
    // 8 packets were delivered and played out
    assert_eq!(played_count, 8);
    // Sequence 3 and 4 were properly reordered without loss
    assert_eq!(jitter_buffer.metrics().packets_played, 8);
    assert_eq!(jitter_buffer.metrics().plc_concealment_events, 2);
}

#[test]
fn device_switch_generation_reset_prevents_stale_buffered_audio() {
    let gen1 = AudioGeneration::INITIAL;
    let mut jb = AudioJitterBuffer::new(gen1, 20);

    let payload = [0xAA; 16];
    let p0 =
        AudioAccessUnit::new(AudioDirection::Downlink, gen1, 0, 0, 480, false, &payload).unwrap();
    let p1 =
        AudioAccessUnit::new(AudioDirection::Downlink, gen1, 1, 480, 480, false, &payload).unwrap();

    assert!(jb.push_packet(p0));
    assert!(jb.push_packet(p1));
    assert_eq!(jb.queued_packet_count(), 2);

    // Host switches audio device or client reconnects: generation advances!
    let gen2 = gen1.next().expect("next generation");
    jb.reset_generation(gen2);

    // Obsolete buffered audio is immediately discarded
    assert_eq!(jb.queued_packet_count(), 0);
    assert_eq!(jb.drain(), JitterDrainResult::Underrun);

    // Late packet from old generation is rejected
    let late_p2 =
        AudioAccessUnit::new(AudioDirection::Downlink, gen1, 2, 960, 480, false, &payload).unwrap();
    assert!(!jb.push_packet(late_p2));

    // Fresh packet from new generation is accepted
    let fresh_p0 =
        AudioAccessUnit::new(AudioDirection::Downlink, gen2, 0, 0, 480, false, &payload).unwrap();
    assert!(jb.push_packet(fresh_p0));
    assert_eq!(jb.queued_packet_count(), 1);
}

#[test]
fn bounded_av_offset_video_never_held_for_delayed_audio() {
    let mut sync = AudioVideoSyncController::new();

    // Video frame is presented at t = 2000 ms
    sync.update_video_presentation(2000);

    // Audio is lagging behind video by 150 ms (at t = 1850 ms -> 1850 * 48 = 88,800 samples)
    let audio_lag_samples = 1850 * 48;
    match sync.check_alignment(audio_lag_samples) {
        AvAlignment::AudioLagging {
            lag_ms,
            drop_samples,
        } => {
            assert_eq!(lag_ms, 150);
            assert_eq!(drop_samples, 150 * 48);
        }
        other => panic!("expected AudioLagging, got {other:?}"),
    }

    // Notice: Video presentation was never blocked, paused, or delayed!
    // Video presentation time advances monotonically to 2016 ms (next frame)
    sync.update_video_presentation(2016);

    // Now audio catches up to t = 2010 ms (skew -6 ms, within tolerance)
    let aligned_audio_samples = 2010 * 48;
    match sync.check_alignment(aligned_audio_samples) {
        AvAlignment::Synchronized { skew_ms } => {
            assert_eq!(skew_ms, -6);
        }
        other => panic!("expected Synchronized, got {other:?}"),
    }
}

#[test]
fn client_volume_control_instant_mute_and_persistence() {
    let mut volume_ctrl = AudioVolumeControl::new();
    assert_eq!(volume_ctrl.volume(), 1.0);
    assert!(!volume_ctrl.is_muted());

    // Instant mute toggle with zero host round trips
    let is_muted = volume_ctrl.toggle_mute();
    assert!(is_muted);
    assert_eq!(volume_ctrl.effective_gain(), 0.0);

    // Software attenuation applied to PCM frame
    let generation = AudioGeneration::INITIAL;
    let samples = [1000i16, 2000i16, -1000i16, -2000i16];
    let mut frame =
        AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 0, &samples).unwrap();

    volume_ctrl.apply_to_pcm(&mut frame);
    assert_eq!(frame.samples(), &[0, 0, 0, 0]);

    // Unmute and set volume to 50%
    volume_ctrl.toggle_mute();
    volume_ctrl.set_volume(0.5);
    assert_eq!(volume_ctrl.effective_gain(), 0.5);

    let mut frame2 =
        AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 0, &samples).unwrap();
    volume_ctrl.apply_to_pcm(&mut frame2);
    assert_eq!(frame2.samples(), &[500, 1000, -500, -1000]);

    // Test per-host persistence store
    let mut store = HostAudioStore::new();
    store.save_settings("host-server.tailnet.net", &volume_ctrl);

    let saved_data = store.save_to_string();
    assert!(saved_data.contains("host-server.tailnet.net=0.5000,0"));

    let loaded_store = HostAudioStore::load_from_str(&saved_data);
    let loaded_ctrl = loaded_store.get_or_default("host-server.tailnet.net");
    assert_eq!(loaded_ctrl.volume(), 0.5);
    assert!(!loaded_ctrl.is_muted());
}
