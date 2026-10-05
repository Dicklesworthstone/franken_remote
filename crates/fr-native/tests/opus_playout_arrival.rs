#![cfg(all(target_os = "linux", feature = "linux-audio"))]
#![forbid(unsafe_code)]
//! Production FRD0 receiver regressions; the decoder is an explicit test double.
//! These test wire/epoch/age admission, not native decoding or audibility.
use fr_client::{
    audio::playout::{PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder},
    input::ClientInstant,
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
    MAX_DECODED_SAMPLES, MAX_JITTER_CEILING_MS, OPUS_SAMPLE_RATE,
};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use fr_native::opus::playout::{Error, OpusPlayout, ReceiveResult};
use fr_wire::audio::{self, AudioConfiguration, AudioPacket, AudioStop};

const BINDING: u32 = 7;
const PAYLOAD: &[u8] = &[0xf8, 0xff, 0xfe];

struct NoDecode;
impl PolledDecoder for NoDecode {
    fn configure(&mut self, _: AudioStreamConfig) -> Result<(), AudioMediaError> {
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, _: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        panic!("expired or refused work must never decode")
    }
    fn submit_plc(&mut self, _: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("expired or refused work must never conceal")
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("expired or refused work must never poll PCM")
    }
}
fn age() -> u64 {
    u64::from(MAX_JITTER_CEILING_MS) * 1000
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
        generation: AudioGeneration::from_raw(1),
        channels: AudioChannels::Stereo,
        sample_rate: OPUS_SAMPLE_RATE,
        frame_duration_ms: 20,
        max_packet_bytes: 64,
        max_decoded_samples: MAX_DECODED_SAMPLES,
        jitter_target_ms: 20,
    }
}
fn owner(now: u64) -> OpusPlayout<NoDecode> {
    OpusPlayout::with_decoder(BINDING, offer(), clock(now), NoDecode).unwrap()
}
fn packet(binding: u32, generation: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut record = vec![0; audio::AUDIO_PACKET_OVERHEAD + payload.len()];
    let size = audio::encode_packet(
        &AudioPacket {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(generation),
            sequence,
            timestamp_samples: sequence * 960,
            duration_samples: 960,
            payload,
        },
        binding,
        &mut record,
    )
    .unwrap();
    record.truncate(size);
    record
}
fn stop(generation: u64) -> Vec<u8> {
    let mut record = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(
        &AudioStop {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(generation),
            reason: AudioStopReason::SessionEnded,
        },
        BINDING,
        &mut record,
    )
    .unwrap();
    record
}
fn render(owner: &mut OpusPlayout<NoDecode>, now: u64) -> Result<PlayoutResult, Error> {
    owner.render(|| Ok(clock(now)), |_, _| panic!("no output expected"))
}

#[test]
fn record_handoff_keeps_original_receipt_deadline() {
    let mut output = owner(0);
    let record = packet(BINDING, 1, 0, PAYLOAD);
    assert_eq!(
        output.receive_record_at(&record, ClientInstant(0), clock(age() / 2)),
        Ok(ReceiveResult::Queued)
    );
    assert_eq!(
        render(&mut output, age()),
        Err(Error::Playout(PlayoutError::Expired))
    );
    assert_eq!(output.queued_packets(), 0);
}

#[test]
fn expired_record_does_not_start_a_device_schedule() {
    let mut output = owner(0);
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 1, 0, PAYLOAD),
            ClientInstant(0),
            clock(age()),
        ),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(output.queued_packets(), 0);
    assert_eq!(output.error(), None);
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 1, 1, PAYLOAD),
            ClientInstant(age() * 2),
            clock(age() * 2),
        ),
        Ok(ReceiveResult::Queued)
    );
}

#[test]
fn duplicate_record_cannot_refresh_a_retained_deadline() {
    let mut output = owner(0);
    let record = packet(BINDING, 1, 0, PAYLOAD);
    output
        .receive_record_at(&record, ClientInstant(0), clock(age() / 4))
        .unwrap();
    assert_eq!(
        output.receive_record_at(&record, ClientInstant(age() / 2), clock(age() / 2)),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(output.queued_packets(), 1);
    assert_eq!(
        render(&mut output, age()),
        Err(Error::Playout(PlayoutError::Expired))
    );
}

#[test]
fn future_receipt_fences_this_epoch() {
    let mut output = owner(0);
    let record = packet(BINDING, 1, 0, PAYLOAD);
    assert_eq!(
        output.receive_record_at(&record, ClientInstant(11), clock(10)),
        Err(Error::Playout(PlayoutError::ClockRegression))
    );
    assert_eq!(output.error(), Some(PlayoutError::ClockRegression));
    assert_eq!(output.queued_packets(), 0);
    assert!(output.receive_record(&record, clock(11)).is_err());
}

#[test]
fn receipt_deadline_overflow_cannot_wrap_into_fresh_audio() {
    let mut output = owner(0);
    assert!(output.poll_configured(clock(0)).unwrap());
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 1, 0, PAYLOAD),
            ClientInstant(u64::MAX - 1),
            clock(u64::MAX - 1),
        ),
        Err(Error::Playout(PlayoutError::ClockOverflow))
    );
    assert_eq!(output.error(), Some(PlayoutError::ClockOverflow));
}

#[test]
fn stale_epoch_cannot_poison_current_epoch_with_a_future_timestamp() {
    let mut output = owner(0);
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 2, 0, PAYLOAD),
            ClientInstant(u64::MAX),
            clock(0),
        ),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(output.error(), None);
    assert_eq!(
        output.receive_record(&packet(BINDING, 1, 0, PAYLOAD), clock(0)),
        Ok(ReceiveResult::Queued)
    );
}

#[test]
fn wrong_binding_is_refused_before_receipt_can_affect_playout() {
    let mut output = owner(0);
    assert!(matches!(
        output.receive_record_at(
            &packet(BINDING + 1, 1, 0, PAYLOAD),
            ClientInstant(u64::MAX),
            clock(0),
        ),
        Err(Error::Wire(_))
    ));
    assert_eq!(output.error(), None);
    assert_eq!(output.queued_packets(), 0);
}

#[test]
fn negotiated_byte_bound_precedes_receipt_and_packet_allocation() {
    let mut output = owner(0);
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 1, 0, &[1; 65]),
            ClientInstant(u64::MAX),
            clock(0),
        ),
        Err(Error::Wire(fr_wire::WireError::ResourceLimit))
    );
    assert_eq!(output.error(), None);
    assert_eq!(output.queued_packets(), 0);
}

#[test]
fn matching_stop_fences_even_with_an_unusable_receipt_clock() {
    let mut output = owner(0);
    output
        .receive_record(&packet(BINDING, 1, 0, PAYLOAD), clock(0))
        .unwrap();
    assert_eq!(
        output.receive_record_at(&stop(1), ClientInstant(u64::MAX), clock(0)),
        Ok(ReceiveResult::Stopped(AudioStopReason::SessionEnded))
    );
    assert_eq!(output.queued_packets(), 0);
    assert!(output.receive_record(&packet(BINDING, 1, 1, PAYLOAD), clock(1)).is_err());
}

#[test]
fn stale_stop_cannot_fence_the_current_queue() {
    let mut output = owner(0);
    output
        .receive_record(&packet(BINDING, 1, 0, PAYLOAD), clock(0))
        .unwrap();
    assert_eq!(
        output.receive_record_at(&stop(2), ClientInstant(u64::MAX), clock(0)),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(output.queued_packets(), 1);
    assert_eq!(output.error(), None);
}

#[test]
fn delayed_handoff_cannot_revive_an_already_expired_queue() {
    let mut output = owner(0);
    output
        .receive_record(&packet(BINDING, 1, 0, PAYLOAD), clock(0))
        .unwrap();
    assert_eq!(
        output.receive_record_at(
            &packet(BINDING, 1, 1, PAYLOAD),
            ClientInstant(age()),
            clock(age()),
        ),
        Err(Error::Playout(PlayoutError::Expired))
    );
    assert_eq!(output.queued_packets(), 0);
}
