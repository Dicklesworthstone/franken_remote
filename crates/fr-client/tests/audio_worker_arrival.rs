//! Worker handoff consumes the original audio age budget. No native codec proof.
use fr_client::{
    audio::playout::{
        AudioPlayout, PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder,
    },
    input::ClientInstant,
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MAX_JITTER_CEILING_MS,
};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};

const AGE: u64 = MAX_JITTER_CEILING_MS as u64 * 1000;

struct Decoder;
impl PolledDecoder for Decoder {
    fn configure(&mut self, _: AudioStreamConfig) -> Result<(), AudioMediaError> {
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, _: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        panic!("expired work must never reach the decoder")
    }
    fn submit_plc(&mut self, _: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("expired work must never reach concealment")
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        panic!("expired work must never poll the decoder")
    }
}
fn clock(now: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(now),
        output_samples: 0,
    }
}
fn owner(now: u64) -> AudioPlayout<Decoder> {
    AudioPlayout::new(
        AudioStreamConfig::new(
            AudioDirection::Downlink,
            AudioGeneration::from_raw(1),
            AudioChannels::Stereo,
            20,
            20,
        )
        .unwrap(),
        Decoder,
        clock(now),
    )
    .unwrap()
}
fn packet(sequence: u64) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(1),
        sequence,
        sequence * 960,
        960,
        false,
        &[0xf8, 0xff, 0xfe],
    )
    .unwrap()
}
fn render(owner: &mut AudioPlayout<Decoder>, now: u64) -> Result<PlayoutResult, PlayoutError> {
    owner.render(|| Ok(clock(now)), |_, _| panic!("no output expected"))
}

#[test]
fn worker_queue_time_is_not_a_fresh_packet_lifetime() {
    let mut owner = owner(0);
    assert!(
        owner
            .receive_at(packet(0), ClientInstant(0), clock(AGE / 2))
            .unwrap()
    );
    assert_eq!(render(&mut owner, AGE - 1), Ok(PlayoutResult::Waiting));
    assert_eq!(render(&mut owner, AGE), Err(PlayoutError::Expired));
    assert_eq!(owner.queued_packets(), 0);
}

#[test]
fn already_expired_handoff_is_dropped_without_starting_playout() {
    let mut owner = owner(0);
    assert!(
        !owner
            .receive_at(packet(0), ClientInstant(0), clock(AGE))
            .unwrap()
    );
    assert_eq!(owner.queued_packets(), 0);
    assert_eq!(owner.error(), None);
    assert_eq!(render(&mut owner, AGE * 2), Ok(PlayoutResult::Waiting));
    assert!(
        owner
            .receive_at(packet(1), ClientInstant(AGE * 2), clock(AGE * 2))
            .unwrap()
    );
    assert_eq!(owner.queued_packets(), 1);
}

#[test]
fn a_duplicate_handoff_cannot_extend_the_first_arrival_deadline() {
    let mut owner = owner(0);
    assert!(
        owner
            .receive_at(packet(0), ClientInstant(0), clock(AGE / 4))
            .unwrap()
    );
    assert!(
        !owner
            .receive_at(packet(0), ClientInstant(AGE / 2), clock(AGE / 2))
            .unwrap()
    );
    assert_eq!(owner.queued_packets(), 1);
    assert_eq!(render(&mut owner, AGE), Err(PlayoutError::Expired));
}

#[test]
fn impossible_future_arrival_is_terminal_not_a_larger_budget() {
    let mut owner = owner(0);
    assert_eq!(
        owner.receive_at(packet(0), ClientInstant(11), clock(10)),
        Err(PlayoutError::ClockRegression)
    );
    assert_eq!(owner.error(), Some(PlayoutError::ClockRegression));
    assert_eq!(owner.queued_packets(), 0);
}

#[test]
fn arrival_deadline_overflow_is_terminal() {
    let mut owner = owner(u64::MAX - 1);
    assert_eq!(
        owner.receive_at(
            packet(0),
            ClientInstant(u64::MAX - 1),
            clock(u64::MAX - 1),
        ),
        Err(PlayoutError::ClockOverflow)
    );
    assert_eq!(owner.error(), Some(PlayoutError::ClockOverflow));
}

#[test]
fn handoff_cannot_revive_already_expired_queued_work() {
    let mut owner = owner(0);
    owner.receive(packet(0), clock(0)).unwrap();
    assert_eq!(
        owner.receive_at(packet(1), ClientInstant(AGE), clock(AGE)),
        Err(PlayoutError::Expired)
    );
    assert_eq!(owner.queued_packets(), 0);
}
