//! Deterministic owner/clock fault tests. The probe decoder is deliberately not
//! an Opus implementation; native codec evidence belongs in fr-native tests.
use fr_client::{
    audio::playout::{AudioPlayout, PlayoutClock, PlayoutError, PlayoutResult},
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioMediaError, AudioPcmFrame};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct Counters {
    decoded: Rc<Cell<usize>>,
    concealed: Rc<Cell<usize>>,
    dropped: Rc<Cell<usize>>,
}
struct Probe {
    config: Option<AudioStreamConfig>,
    pcm: Option<AudioPcmFrame>,
    next: u64,
    counts: Counters,
    wrong_pcm: bool,
    reject_config: bool,
    panic_decode: bool,
}
impl Probe {
    fn new(counts: Counters) -> Self {
        Self {
            config: None,
            pcm: None,
            next: 0,
            counts,
            wrong_pcm: false,
            reject_config: false,
            panic_decode: false,
        }
    }
    fn frame(&self, at: u64) -> AudioPcmFrame {
        let c = self.config.unwrap();
        let generation = if self.wrong_pcm {
            c.generation().next().unwrap()
        } else {
            c.generation()
        };
        AudioPcmFrame::from_interleaved(
            generation,
            c.channels(),
            at,
            &vec![
                8000;
                c.expected_samples_per_frame() as usize * usize::from(c.channels().count())
            ],
        )
        .unwrap()
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.dropped.set(self.counts.dropped.get() + 1);
    }
}
impl AudioDecoder for Probe {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        if self.reject_config {
            return Err(AudioMediaError::UnsupportedFormat);
        }
        self.config = Some(config);
        self.pcm = None;
        Ok(())
    }
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        assert!(!self.panic_decode, "deliberate worker unwind");
        self.counts.decoded.set(self.counts.decoded.get() + 1);
        self.next = packet.timestamp_samples() + u64::from(packet.duration_samples());
        self.pcm = Some(self.frame(packet.timestamp_samples()));
        Ok(())
    }
    fn decode_plc(&mut self, samples: u16) -> Result<AudioPcmFrame, AudioMediaError> {
        self.counts.concealed.set(self.counts.concealed.get() + 1);
        let frame = self.frame(self.next);
        self.next += u64::from(samples);
        Ok(frame)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        Ok(self.pcm.take())
    }
    fn reset(&mut self, generation: AudioGeneration) {
        let _ = generation;
        panic!("owner must replace rather than relabel decoder state");
    }
}
fn config(
    generation: u64,
    direction: AudioDirection,
    duration: u16,
    target: u16,
) -> AudioStreamConfig {
    AudioStreamConfig::new(
        direction,
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        duration,
        target,
    )
    .unwrap()
}
fn clock(micros: u64, samples: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(micros),
        output_samples: samples,
    }
}
fn packet(c: AudioStreamConfig, seq: u64) -> AudioAccessUnit {
    let count = u16::try_from(c.expected_samples_per_frame()).unwrap();
    // Remote source origin deliberately differs from BOTH local clocks.
    AudioAccessUnit::new(
        c.direction(),
        c.generation(),
        seq,
        900_000 + seq * u64::from(count),
        count,
        false,
        &[0x42],
    )
    .unwrap()
}
fn render(
    owner: &mut AudioPlayout<Probe>,
    at: PlayoutClock,
) -> Result<PlayoutResult, PlayoutError> {
    owner.render(|| Ok(at), |_, _| Ok(()))
}
fn setup() -> (AudioPlayout<Probe>, Counters, AudioStreamConfig) {
    let c = config(7, AudioDirection::Downlink, 10, 20);
    let counts = Counters::default();
    let owner = AudioPlayout::new(c, Probe::new(counts.clone()), clock(1_000_000, 50_000)).unwrap();
    (owner, counts, c)
}
#[test]
fn device_clock_paces_real_packets_and_plc_without_poll_frequency_dependence() {
    let (mut owner, count, c) = setup();
    for seq in [0, 2, 3] {
        assert!(
            owner
                .receive(packet(c, seq), clock(1_000_000, 50_000))
                .unwrap()
        );
    }
    assert_eq!(
        render(&mut owner, clock(1_020_000, 50_959)).unwrap(),
        PlayoutResult::Waiting
    );
    assert_eq!(count.decoded.get(), 0);
    for seq in 0..4 {
        let now = clock(1_020_000 + seq * 10_000, 50_960 + seq * 480);
        let PlayoutResult::Submitted(receipt) = render(&mut owner, now).unwrap() else {
            panic!("due");
        };
        assert_eq!(receipt.sequence, seq);
        assert_eq!(receipt.source_samples, 900_000 + seq * 480);
        assert_eq!(receipt.output_samples, 50_960 + seq * 480);
        assert_eq!(receipt.concealed, seq == 1);
        for _ in 0..20 {
            assert_eq!(render(&mut owner, now).unwrap(), PlayoutResult::Waiting);
        }
    }
    assert_eq!(count.decoded.get(), 3);
    assert_eq!(count.concealed.get(), 1);
}
#[test]
fn stalled_device_and_duplicate_flood_cannot_keep_audio_alive() {
    let (mut owner, count, c) = setup();
    owner
        .receive(packet(c, 0), clock(1_000_000, 50_000))
        .unwrap();
    for offset in 1..100 {
        assert!(
            !owner
                .receive(packet(c, 0), clock(1_000_000 + offset * 1000, 50_000))
                .unwrap()
        );
    }
    assert_eq!(
        owner.receive(packet(c, 1), clock(1_100_000, 50_000)),
        Err(PlayoutError::Expired)
    );
    assert_eq!(owner.queued_packets(), 0);
    assert_eq!(count.dropped.get(), 1);
    assert_eq!(count.decoded.get(), 0);
    assert_eq!(
        render(&mut owner, clock(1_101_000, 50_960)),
        Err(PlayoutError::Expired)
    );
}
#[test]
fn delayed_service_never_bursts_through_old_device_slots() {
    let (mut owner, count, c) = setup();
    for seq in 0..5 {
        owner
            .receive(packet(c, seq), clock(1_000_000, 50_000))
            .unwrap();
    }
    assert_eq!(
        render(&mut owner, clock(1_030_000, 51_440)),
        Err(PlayoutError::MissedDeviceSlot)
    );
    assert_eq!(count.decoded.get(), 0);
    assert_eq!(count.dropped.get(), 1);
    assert_eq!(owner.queued_packets(), 0);
}
#[test]
fn expired_startup_packets_cannot_be_refreshed_by_newer_startup_evictions() {
    let (mut owner, count, c) = setup();
    for seq in 0..20 {
        owner
            .receive(packet(c, seq), clock(1_000_000 + seq * 4000, 50_000))
            .unwrap();
    }
    assert_eq!(
        render(&mut owner, clock(1_100_000, 50_960)),
        Err(PlayoutError::Expired)
    );
    assert_eq!(count.decoded.get(), 0);
}
#[test]
fn permission_is_checked_before_decode_and_again_before_submission() {
    for revoke_at in [0, 1] {
        let (mut owner, count, c) = setup();
        for seq in 0..2 {
            owner
                .receive(packet(c, seq), clock(1_000_000, 50_000))
                .unwrap();
        }
        let mut checks = 0;
        let mut submitted = 0;
        assert_eq!(
            owner.render(
                || {
                    let this = checks;
                    checks += 1;
                    if this == revoke_at {
                        Err(PlayoutError::Denied)
                    } else {
                        Ok(clock(1_020_000, 50_960))
                    }
                },
                |_, _| {
                    submitted += 1;
                    Ok(())
                }
            ),
            Err(PlayoutError::Denied)
        );
        assert_eq!(checks, revoke_at + 1);
        assert_eq!(submitted, 0);
        assert_eq!(count.decoded.get(), revoke_at);
        assert_eq!(owner.queued_packets(), 0);
        assert_eq!(count.dropped.get(), 1);
    }
}
#[test]
fn decode_latency_cannot_refresh_packet_age_or_device_slot() {
    for (late, expected) in [
        (clock(1_100_000, 50_960), PlayoutError::Expired),
        (clock(1_030_000, 51_440), PlayoutError::MissedDeviceSlot),
    ] {
        let (mut owner, count, c) = setup();
        for seq in 0..2 {
            owner
                .receive(packet(c, seq), clock(1_000_000, 50_000))
                .unwrap();
        }
        let mut checks = 0;
        assert_eq!(
            owner.render(
                || {
                    checks += 1;
                    Ok(if checks == 1 {
                        clock(1_020_000, 50_960)
                    } else {
                        late
                    })
                },
                |_, _| panic!("stale PCM reached output")
            ),
            Err(expected)
        );
        assert_eq!(count.decoded.get(), 1);
        assert_eq!(count.dropped.get(), 1);
    }
}
#[test]
fn volume_and_mute_are_applied_at_local_submission_not_packet_arrival() {
    let (mut owner, _, c) = setup();
    for seq in 0..3 {
        owner
            .receive(packet(c, seq), clock(1_000_000, 50_000))
            .unwrap();
    }
    owner.volume_mut().set_volume(0.25);
    owner
        .render(
            || Ok(clock(1_020_000, 50_960)),
            |pcm, _| {
                assert!(pcm.samples().iter().all(|&s| s == 2000));
                Ok(())
            },
        )
        .unwrap();
    owner.volume_mut().set_muted(true);
    owner
        .render(
            || Ok(clock(1_030_000, 51_440)),
            |pcm, _| {
                assert!(pcm.samples().iter().all(|&s| s == 0));
                Ok(())
            },
        )
        .unwrap();
}
#[test]
fn output_backpressure_or_unwind_is_terminal_not_a_replay_request() {
    for panic_output in [false, true] {
        let (mut owner, count, c) = setup();
        for seq in 0..2 {
            owner
                .receive(packet(c, seq), clock(1_000_000, 50_000))
                .unwrap();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.render(
                || Ok(clock(1_020_000, 50_960)),
                |_, _| {
                    assert!(!panic_output, "output unwind");
                    Err(PlayoutError::Output)
                },
            )
        }));
        if panic_output {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(PlayoutError::Output));
        }
        assert!(owner.error().is_some());
        assert_eq!(count.dropped.get(), 1);
        assert_eq!(owner.queued_packets(), 0);
    }
}
#[test]
fn wrong_codec_output_and_codec_unwind_never_reach_submission() {
    for panic_decode in [false, true] {
        let c = config(7, AudioDirection::Downlink, 10, 10);
        let counts = Counters::default();
        let mut decoder = Probe::new(counts.clone());
        decoder.wrong_pcm = true;
        decoder.panic_decode = panic_decode;
        let mut owner = AudioPlayout::new(c, decoder, clock(0, 0)).unwrap();
        owner.receive(packet(c, 0), clock(0, 0)).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.render(|| Ok(clock(10_000, 480)), |_, _| panic!("bad output"))
        }));
        if panic_decode {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(PlayoutError::CodecOutput));
        }
        assert!(owner.error().is_some());
        assert_eq!(counts.dropped.get(), 1);
    }
}
#[test]
fn clock_regression_on_either_axis_is_terminal() {
    for regressed in [clock(999_999, 50_000), clock(1_000_000, 49_999)] {
        let (mut owner, count, c) = setup();
        owner
            .receive(packet(c, 0), clock(1_000_000, 50_000))
            .unwrap();
        assert_eq!(
            render(&mut owner, regressed),
            Err(PlayoutError::ClockRegression)
        );
        assert_eq!(count.dropped.get(), 1);
    }
}
#[test]
fn every_negotiated_direction_and_duration_is_paced_in_sample_units() {
    for duration in [5, 10, 20, 40, 60, 80, 100] {
        for direction in [AudioDirection::Downlink, AudioDirection::Uplink] {
            let target = duration.min(80);
            let c = config(1, direction, duration, target);
            let counts = Counters::default();
            let mut owner = AudioPlayout::new(c, Probe::new(counts.clone()), clock(0, 0)).unwrap();
            owner.receive(packet(c, 0), clock(0, 0)).unwrap();
            let due = u64::from(target) * 48;
            assert_eq!(
                render(&mut owner, clock(u64::from(target) * 1000, due - 1)).unwrap(),
                PlayoutResult::Waiting
            );
            let PlayoutResult::Submitted(receipt) =
                render(&mut owner, clock(u64::from(target) * 1000, due)).unwrap()
            else {
                panic!("due");
            };
            assert_eq!(receipt.direction, direction);
            assert_eq!(counts.decoded.get(), 1);
        }
    }
}
#[test]
fn failed_reset_never_revives_old_sound_and_requires_another_new_epoch() {
    let (mut owner, old, c) = setup();
    owner
        .receive(packet(c, 0), clock(1_000_000, 50_000))
        .unwrap();
    assert_eq!(
        owner.reconfigure(c, Probe::new(Counters::default()), clock(0, 0)),
        Err(PlayoutError::GenerationNotAdvanced)
    );
    assert_eq!(old.dropped.get(), 1);
    assert_eq!(owner.generation(), c.generation());
    let next = config(8, c.direction(), 10, 20);
    let failed = Counters::default();
    let mut decoder = Probe::new(failed.clone());
    decoder.reject_config = true;
    assert_eq!(
        owner.reconfigure(next, decoder, clock(0, 0)),
        Err(PlayoutError::Codec(AudioMediaError::UnsupportedFormat))
    );
    assert_eq!(owner.generation(), next.generation());
    assert_eq!(failed.dropped.get(), 1);
    assert_eq!(
        owner.reconfigure(next, Probe::new(Counters::default()), clock(0, 0)),
        Err(PlayoutError::GenerationNotAdvanced)
    );
    let fresh = config(9, c.direction(), 10, 10);
    owner
        .reconfigure(fresh, Probe::new(Counters::default()), clock(0, 0))
        .unwrap();
    assert!(!owner.receive(packet(c, 0), clock(0, 0)).unwrap());
    owner.receive(packet(fresh, 0), clock(0, 0)).unwrap();
    assert!(matches!(
        render(&mut owner, clock(10_000, 480)),
        Ok(PlayoutResult::Submitted(_))
    ));
}
#[test]
fn downlink_reconfiguration_never_enables_the_microphone_direction() {
    let (mut owner, count, _) = setup();
    assert_eq!(
        owner.reconfigure(
            config(8, AudioDirection::Uplink, 10, 20),
            Probe::new(Counters::default()),
            clock(0, 0)
        ),
        Err(PlayoutError::Configuration)
    );
    assert_eq!(count.dropped.get(), 1);
    assert!(owner.error().is_some());
}
#[test]
fn overflowing_deadlines_and_output_cursors_refuse_without_wrapping() {
    for start in [clock(u64::MAX - 99_999, 0), clock(0, u64::MAX - 479)] {
        let c = config(0, AudioDirection::Downlink, 10, 10);
        let counts = Counters::default();
        let mut owner = AudioPlayout::new(c, Probe::new(counts.clone()), start).unwrap();
        assert_eq!(
            owner.receive(packet(c, 0), start),
            Err(PlayoutError::ClockOverflow)
        );
        assert_eq!(counts.dropped.get(), 1);
    }
}
#[test]
fn dropping_owner_releases_decoder_and_stop_is_idempotent() {
    let (mut owner, count, c) = setup();
    owner
        .receive(packet(c, 0), clock(1_000_000, 50_000))
        .unwrap();
    owner.stop();
    owner.stop();
    assert_eq!(owner.queued_packets(), 0);
    assert_eq!(count.dropped.get(), 1);
    drop(owner);
    assert_eq!(count.dropped.get(), 1);
    let (owner, count, _) = setup();
    drop(owner);
    assert_eq!(count.dropped.get(), 1);
}
