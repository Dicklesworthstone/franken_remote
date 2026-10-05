#![cfg(all(target_os = "linux", feature = "linux-audio-playout"))]
//! Real wire/owner composition with a non-decoding fixture, not codec or device proof.
use fr_client::{
    audio::playout::{PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder},
    input::ClientInstant,
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
    MAX_JITTER_CEILING_MS,
};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use fr_native::opus::playout::{Error, OpusPlayout, ReceiveResult};
use fr_wire::audio::{self, AudioConfiguration, AudioPacket, AudioStop};

const BINDING: u32 = 73;
const AGE: u64 = MAX_JITTER_CEILING_MS as u64 * 1000;

struct NeverDecode;
impl PolledDecoder for NeverDecode {
    fn configure(&mut self, _: AudioStreamConfig) -> Result<(), AudioMediaError> {
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, _: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        panic!("obsolete records must never reach decode")
    }
    fn submit_plc(&mut self, _: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("obsolete records must never cause concealment")
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("obsolete records must never poll decode")
    }
}
fn clock(now: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(now),
        output_samples: 0,
    }
}
fn offer() -> AudioConfiguration {
    AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Stereo,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 1275,
        max_decoded_samples: 960,
        jitter_target_ms: 20,
    }
}
fn owner() -> OpusPlayout<NeverDecode> {
    OpusPlayout::with_decoder(BINDING, offer(), clock(0), NeverDecode).unwrap()
}
fn packet(sequence: u64, generation: u64, binding: u32) -> Vec<u8> {
    let packet = AudioPacket {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(generation),
        sequence,
        timestamp_samples: sequence * 960,
        duration_samples: 960,
        payload: &[0xf8, 0xff, 0xfe],
    };
    let mut record = vec![0; audio::AUDIO_PACKET_OVERHEAD + packet.payload.len()];
    let n = audio::encode_packet(&packet, binding, &mut record).unwrap();
    record.truncate(n);
    record
}
fn render(owner: &mut OpusPlayout<NeverDecode>, now: u64) -> Result<PlayoutResult, Error> {
    owner.render(|| Ok(clock(now)), |_, _| panic!("unexpected output"))
}

#[test]
fn wire_handoff_does_not_restart_the_original_packet_lifetime() {
    let mut owner = owner();
    assert_eq!(
        owner.receive_record_at(&packet(0, 7, BINDING), ClientInstant(0), clock(AGE / 2)),
        Ok(ReceiveResult::Queued)
    );
    assert_eq!(render(&mut owner, AGE - 1), Ok(PlayoutResult::Waiting));
    assert_eq!(
        render(&mut owner, AGE),
        Err(Error::Playout(PlayoutError::Expired))
    );
    assert_eq!(owner.queued_packets(), 0);
}

#[test]
fn exact_expiry_is_ignored_without_starting_a_device_schedule() {
    let mut owner = owner();
    assert_eq!(
        owner.receive_record_at(&packet(0, 7, BINDING), ClientInstant(0), clock(AGE)),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(owner.error(), None);
    assert_eq!(owner.queued_packets(), 0);
    assert_eq!(render(&mut owner, AGE * 2), Ok(PlayoutResult::Waiting));
    assert_eq!(
        owner.receive_record_at(
            &packet(1, 7, BINDING),
            ClientInstant(AGE * 2),
            clock(AGE * 2),
        ),
        Ok(ReceiveResult::Queued)
    );
}

#[test]
fn duplicate_wire_arrival_cannot_refresh_queued_audio() {
    let mut owner = owner();
    let record = packet(0, 7, BINDING);
    assert_eq!(
        owner.receive_record_at(&record, ClientInstant(0), clock(AGE / 4)),
        Ok(ReceiveResult::Queued)
    );
    assert_eq!(
        owner.receive_record_at(&record, ClientInstant(AGE / 2), clock(AGE / 2)),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(
        render(&mut owner, AGE),
        Err(Error::Playout(PlayoutError::Expired))
    );
}

#[test]
fn expired_handoff_still_checks_channel_binding_and_wire_shape() {
    let mut owner = owner();
    assert!(matches!(
        owner.receive_record_at(&packet(0, 7, BINDING + 1), ClientInstant(0), clock(AGE)),
        Err(Error::Wire(_))
    ));
    assert!(matches!(
        owner.receive_record_at(&[0; 3], ClientInstant(0), clock(AGE)),
        Err(Error::Wire(_))
    ));
    assert_eq!(owner.queued_packets(), 0);
}

#[test]
fn stale_epoch_cannot_retire_or_refresh_the_current_epoch() {
    let mut owner = owner();
    assert_eq!(
        owner.receive_record_at(&packet(0, 6, BINDING), ClientInstant(AGE), clock(0)),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(owner.error(), None);
    assert_eq!(
        owner.receive_record_at(&packet(0, 7, BINDING), ClientInstant(0), clock(0)),
        Ok(ReceiveResult::Queued)
    );
}

#[test]
fn matching_stop_is_not_discarded_as_an_obsolete_packet() {
    let mut owner = owner();
    owner
        .receive_record(&packet(0, 7, BINDING), clock(0))
        .unwrap();
    let stop = AudioStop {
        direction: AudioDirection::Downlink,
        generation: offer().generation,
        reason: AudioStopReason::SessionEnded,
    };
    let mut record = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(&stop, BINDING, &mut record).unwrap();
    assert_eq!(
        owner.receive_record_at(&record, ClientInstant(0), clock(AGE * 2)),
        Ok(ReceiveResult::Stopped(AudioStopReason::SessionEnded))
    );
    assert_eq!(owner.queued_packets(), 0);
    assert!(owner.error().is_some());
}

#[test]
fn a_future_local_arrival_is_terminal_not_extra_playback_time() {
    let mut owner = owner();
    assert_eq!(
        owner.receive_record_at(&packet(0, 7, BINDING), ClientInstant(11), clock(10)),
        Err(Error::Playout(PlayoutError::ClockRegression))
    );
    assert_eq!(owner.error(), Some(PlayoutError::ClockRegression));
}
