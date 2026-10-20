//! Production jitter/playout and PCM correction under independently advancing
//! source/device clocks. The single-result codec and arrivals are explicit fixtures.
use fr_client::{
    audio::playout::{
        AudioPlayout, PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder,
    },
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};

#[derive(Default)]
struct Decoder {
    config: Option<AudioStreamConfig>,
    pcm: Option<AudioPcmFrame>,
    source_next: u64,
}
impl Decoder {
    fn frame(&mut self, at: u64, concealed: bool) -> AudioPcmFrame {
        let config = self.config.unwrap();
        self.source_next = at + u64::from(config.expected_samples_per_frame());
        let mut data = [0_i16; 1920];
        let count =
            config.expected_samples_per_frame() as usize * usize::from(config.channels().count());
        for (i, sample) in data[..count].iter_mut().enumerate() {
            *sample = if concealed {
                0
            } else if i % 2 == 0 {
                12_345
            } else {
                -12_345
            };
        }
        AudioPcmFrame::from_interleaved(config.generation(), config.channels(), at, &data[..count])
            .unwrap()
    }
}
impl PolledDecoder for Decoder {
    fn configure(&mut self, c: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(c);
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, p: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        assert!(self.pcm.is_none(), "one native result only");
        self.pcm = Some(self.frame(p.timestamp_samples(), false));
        Ok(())
    }
    fn submit_plc(&mut self, _: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        assert!(self.pcm.is_none());
        Ok(Some(self.frame(self.source_next, true)))
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        Ok(self.pcm.take())
    }
}
fn config(duration: u16, channels: AudioChannels) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::INITIAL,
        channels,
        duration,
        20,
    )
    .unwrap()
}
fn packet(c: AudioStreamConfig, seq: u64) -> AudioAccessUnit {
    let samples = u16::try_from(c.expected_samples_per_frame()).unwrap();
    AudioAccessUnit::new(
        c.direction(),
        c.generation(),
        seq,
        900_000 + seq * u64::from(samples),
        samples,
        false,
        &[0x42],
    )
    .unwrap()
}
fn clock(now: u64, device_ppm: i32) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(now),
        output_samples: u64::try_from(
            u128::from(now) * 48 * u128::try_from(1_000_000 + device_ppm).unwrap() / 1_000_000_000,
        )
        .unwrap(),
    }
}

#[derive(Debug)]
struct Stats {
    submitted: u64,
    adjusted: u64,
    concealed: u64,
    max_queue: usize,
    ppm: i32,
}
fn exercise(
    duration: u16,
    channels: AudioChannels,
    source_ppm: i32,
    device_ppm: i32,
    seconds: u64,
    jitter: bool,
    loss: bool,
) -> Stats {
    let c = config(duration, channels);
    let mut owner = AudioPlayout::new(c, Decoder::default(), clock(0, device_ppm)).unwrap();
    let frame_us = u64::from(duration) * 1000;
    let total = seconds * 1_000_000;
    let mut events = Vec::new();
    for seq in 0..(seconds * 1000 / u64::from(duration) + 100) {
        if loss && seq > 200 && seq % 97 == 0 {
            continue;
        }
        let noise = if jitter {
            [0, 2000, 6000, 1000, 3000][usize::try_from(seq % 5).unwrap()]
        } else {
            0
        };
        let at = u64::try_from(
            u128::from(seq * frame_us) * 1_000_000
                / u128::try_from(1_000_000 + source_ppm).unwrap(),
        )
        .unwrap()
            + noise;
        events.push((at, seq, false));
        if jitter && seq % 13 == 0 {
            events.push((at + 1000, seq, true));
        }
    }
    events.sort_unstable();
    let mut pos = 0;
    let mut stats = Stats {
        submitted: 0,
        adjusted: 0,
        concealed: 0,
        max_queue: 0,
        ppm: 0,
    };
    let mut next_output = None;
    for now in (0..total).step_by(1000) {
        let current = clock(now, device_ppm);
        while pos < events.len() && events[pos].0 <= now {
            let (arrived, seq, duplicate) = events[pos];
            let accepted = owner
                .receive_at(packet(c, seq), ClientInstant(arrived), current)
                .unwrap_or_else(|e| panic!("receive at {now}, seq {seq}, stats {stats:?}: {e:?}"));
            if duplicate {
                assert!(!accepted);
            }
            pos += 1;
        }
        stats.max_queue = stats.max_queue.max(owner.queued_packets());
        let result = owner
            .render(
                || Ok(current),
                |pcm, r| {
                    assert_eq!(r.sequence, stats.submitted);
                    assert_eq!(
                        r.source_samples,
                        900_000 + r.sequence * u64::from(c.expected_samples_per_frame())
                    );
                    assert!(
                        r.output_samples <= current.output_samples
                            && current.output_samples < r.output_valid_before
                    );
                    assert!(r.valid_until.0 > now);
                    assert_eq!(
                        u64::try_from(pcm.samples_per_channel()).unwrap(),
                        r.output_valid_before - r.output_samples
                    );
                    assert!(
                        pcm.samples_per_channel()
                            .abs_diff(c.expected_samples_per_frame() as usize)
                            <= 1
                    );
                    if let Some(next) = next_output {
                        assert_eq!(r.output_samples, next, "no output gaps/overlaps");
                    }
                    next_output = Some(r.output_valid_before);
                    stats.submitted += 1;
                    stats.adjusted += u64::from(
                        pcm.samples_per_channel() != c.expected_samples_per_frame() as usize,
                    );
                    stats.concealed += u64::from(r.concealed);
                    Ok(())
                },
            )
            .unwrap_or_else(|e| panic!("render at {now}, stats {stats:?}: {e:?}"));
        assert!(matches!(
            result,
            PlayoutResult::Waiting | PlayoutResult::Submitted(_)
        ));
        assert!(owner.clock_correction_ppm().abs() <= 1000);
    }
    stats.ppm = owner.clock_correction_ppm();
    println!(
        "duration={duration}ms source_ppm={source_ppm} device_ppm={device_ppm} seconds={seconds} jitter={jitter} loss={loss}: {stats:?}"
    );
    assert!(owner.error().is_none());
    stats
}

#[test]
fn six_minute_source_device_clock_skew_does_not_accumulate_or_lose_real_packets() {
    for duration in [10, 20] {
        for (source, device) in [(600, 0), (0, 600)] {
            let stats = exercise(
                duration,
                AudioChannels::Stereo,
                source,
                device,
                360,
                false,
                false,
            );
            assert_eq!(stats.concealed, 0, "{stats:?}");
            assert!(
                stats.adjusted > 1000,
                "correction is actually exercised: {stats:?}"
            );
            assert!(
                stats.max_queue <= 5,
                "bounded latency rather than backlog: {stats:?}"
            );
            assert_eq!(stats.ppm.signum(), (device - source).signum());
        }
    }
}
#[test]
fn equal_clocks_keep_bit_exact_duration_in_mono_and_stereo() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        let stats = exercise(10, channels, 0, 0, 15, false, false);
        assert_eq!(stats.adjusted, 0);
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.ppm, 0);
    }
}
#[test]
fn delayed_duplicate_arrivals_and_isolated_loss_do_not_renew_or_reset_the_timeline() {
    for (source, device) in [(500, 0), (0, 500)] {
        let stats = exercise(10, AudioChannels::Stereo, source, device, 180, true, true);
        assert!(stats.adjusted > 500, "{stats:?}");
        let injected = ((stats.submitted - 1) / 97).saturating_sub(200 / 97);
        assert_eq!(
            stats.concealed, injected,
            "conceal exactly the injected loss, never a clock slip: {stats:?}"
        );
        assert!(stats.max_queue <= 5, "{stats:?}");
    }
}
#[test]
fn correction_cannot_relabel_a_missed_device_slot_or_resurrect_a_stopped_owner() {
    let c = config(10, AudioChannels::Mono);
    let mut owner = AudioPlayout::new(c, Decoder::default(), clock(0, 0)).unwrap();
    owner.receive(packet(c, 0), clock(0, 0)).unwrap();
    owner.receive(packet(c, 1), clock(10_000, 0)).unwrap();
    assert!(matches!(
        owner
            .render(|| Ok(clock(20_000, 0)), |_, _| Ok(()))
            .unwrap(),
        PlayoutResult::Submitted(_)
    ));
    assert_eq!(
        owner.render(
            || Ok(clock(40_000, 0)),
            |_, _| panic!("obsolete device slot")
        ),
        Err(PlayoutError::MissedDeviceSlot)
    );
    assert_eq!(owner.clock_correction_ppm(), 0);
    assert!(owner.receive(packet(c, 2), clock(40_001, 0)).is_err());
}
