use super::*;
use fr_client::audio::playout::drift::Drift;
use fr_core::audio::{AudioChannels, AudioGeneration};
use std::{cell::Cell, time::Duration};

fn configuration(duration: u16, channels: AudioChannels) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(7),
        channels,
        duration,
        20,
    )
    .unwrap()
}

fn frame(config: AudioStreamConfig, source: u64, expires: Instant) -> Frame {
    let count = usize::try_from(config.expected_samples_per_frame()).unwrap()
        * usize::from(config.channels().count());
    Frame {
        pcm: Pcm(AudioPcmFrame::from_interleaved(
            config.generation(),
            config.channels(),
            source,
            &vec![17; count],
        )
        .unwrap()),
        span: Drift::default().plan(config.expected_samples_per_frame()),
        expires,
    }
}

fn complete(quantum: u32, now: Instant) -> Startup {
    let config = configuration(10, AudioChannels::Mono);
    let mut startup = Startup::new(config).unwrap();
    let mut source = 1_000_000;
    while startup.needs_frame(quantum).unwrap() {
        startup
            .push(
                frame(config, source, now + Duration::from_millis(50)),
                quantum,
                now,
            )
            .unwrap();
        source += 480;
    }
    startup
}

#[test]
fn complete_quantum_uses_minimum_real_frames_and_one_absolute_interval() {
    let now = Instant::now();
    for quantum in [256, 512, 1024] {
        for duration in [10, 20] {
            for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
                let config = configuration(duration, channels);
                let nominal = config.expected_samples_per_frame();
                let mut startup = Startup::new(config).unwrap();
                let mut source = 1_000_000;
                while startup.needs_frame(quantum).unwrap() {
                    assert!(startup.take(quantum, 4, now).unwrap().is_none());
                    startup
                        .push(
                            frame(config, source, now + Duration::from_millis(50)),
                            quantum,
                            now,
                        )
                        .unwrap();
                    source += u64::from(nominal);
                }
                let count = usize::try_from(quantum.div_ceil(nominal)).unwrap();
                assert_eq!(startup.frames.len(), count);
                assert!(count <= MAX_FRAMES);
                assert!(startup.samples <= MAX_SAMPLES);
                assert_eq!(
                    startup.bytes,
                    usize::try_from(startup.samples).unwrap()
                        * usize::from(channels.count())
                        * size_of::<i16>()
                );
                assert!(startup.bytes <= MAX_BYTES);
                // Capacity pressure neither consumes PCM nor permits a fourth
                // decode while a complete first quantum is already prepared.
                assert!(startup.take(quantum, count - 1, now).unwrap().is_none());
                assert!(!startup.needs_frame(quantum).unwrap());
                let batch = startup.take(quantum, count, now).unwrap().unwrap();
                assert!(startup.is_empty());
                let start = 9_000_000; // Chosen only when the complete batch is ready.
                let mut admissions = Vec::new();
                let result = batch
                    .admit::<Error>(
                        start,
                        || now,
                        |frame, at| {
                            assert!(frame.pcm.0.samples().iter().all(|sample| *sample == 17));
                            assert_eq!(frame.pcm.0.channels(), channels);
                            assert_eq!(frame.expires, now + Duration::from_millis(50));
                            admissions.push((frame.pcm.0.timestamp_samples(), at));
                            Ok(())
                        },
                    )
                    .unwrap();
                assert_eq!(admissions.len(), count);
                for (index, (source, at)) in admissions.into_iter().enumerate() {
                    let offset = u64::try_from(index).unwrap() * u64::from(nominal);
                    assert_eq!(source, 1_000_000 + offset);
                    assert_eq!(at, start + offset);
                    assert!(at < start + u64::from(quantum));
                }
                assert_eq!(result.source_samples, 1_000_000);
                assert_eq!(
                    result.output_samples,
                    u32::try_from(count).unwrap() * nominal
                );
                assert!(result.output_samples >= quantum);
                assert_eq!(startup.needs_frame(quantum), Err(Error::Metadata));
                assert_eq!(
                    startup.push(
                        frame(config, source, now + Duration::from_millis(50)),
                        quantum,
                        now,
                    ),
                    Err(Error::Metadata),
                    "successful startup cannot be rearmed for another output anchor"
                );
            }
        }
    }
}

#[test]
fn original_earliest_expiry_survives_newer_frames_and_capacity_waits() {
    let now = Instant::now();
    let until = now + Duration::from_millis(10);
    let config = configuration(10, AudioChannels::Mono);
    let mut startup = Startup::new(config).unwrap();
    startup.push(frame(config, 1000, until), 512, now).unwrap();
    startup
        .push(
            frame(config, 1480, now + Duration::from_millis(90)),
            512,
            now + Duration::from_millis(5),
        )
        .unwrap();
    assert!(
        startup
            .take(512, 1, until.checked_sub(Duration::from_nanos(1)).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(startup.frames[0].expires, until);
    assert_eq!(startup.check_live(until), Err(Error::Expired));
    assert!(startup.is_empty());
    assert_eq!(startup.bytes, 0);
    assert_eq!(startup.samples, 0);
    assert_eq!(startup.needs_frame(512), Err(Error::Metadata));
}

#[test]
fn incomplete_quantum_expires_without_any_native_admission() {
    let now = Instant::now();
    let config = configuration(10, AudioChannels::Stereo);
    let mut startup = Startup::new(config).unwrap();
    let until = now + Duration::from_millis(3);
    startup.push(frame(config, 0, until), 1024, now).unwrap();
    assert!(startup.take(1024, 4, now).unwrap().is_none());
    assert!(matches!(startup.take(1024, 4, until), Err(Error::Expired)));
    assert!(startup.is_empty());
    assert_eq!(startup.needs_frame(1024), Err(Error::Metadata));
}

#[test]
fn gaps_overlaps_generation_format_and_quantum_changes_cannot_join_startup() {
    let now = Instant::now();
    let expires = now + Duration::from_millis(50);
    let config = configuration(10, AudioChannels::Mono);
    for case in 0..7 {
        let mut startup = Startup::new(config).unwrap();
        startup
            .push(frame(config, 1000, expires), 1024, now)
            .unwrap();
        let mut next = frame(config, 1480, expires);
        let mut quantum = 1024;
        match case {
            0 => next = frame(config, 1481, expires),
            1 => next = frame(config, 1479, expires),
            2 => {
                let foreign = AudioStreamConfig::new(
                    AudioDirection::Uplink,
                    AudioGeneration::from_raw(8),
                    AudioChannels::Mono,
                    10,
                    20,
                )
                .unwrap();
                next = frame(foreign, 1480, expires);
            }
            3 => next = frame(configuration(10, AudioChannels::Stereo), 1480, expires),
            4 => next = frame(configuration(20, AudioChannels::Mono), 1480, expires),
            5 => next.span = Drift::default().plan(960),
            6 => quantum = 512,
            _ => unreachable!(),
        }
        assert_eq!(
            startup.push(next, quantum, now),
            Err(Error::Metadata),
            "case {case}"
        );
        assert!(startup.is_empty());
        assert_eq!(startup.needs_frame(1024), Err(Error::Metadata));
    }
}

#[test]
fn complete_startup_refuses_additional_pcm_and_unsupported_quanta() {
    let now = Instant::now();
    let config = configuration(10, AudioChannels::Mono);
    let mut startup = complete(1024, now);
    assert_eq!(startup.frames.len(), 3);
    assert_eq!(
        startup.push(
            frame(config, 1_001_440, now + Duration::from_millis(50)),
            1024,
            now
        ),
        Err(Error::Capacity)
    );
    assert!(startup.is_empty());
    for quantum in [0, 1025] {
        let mut startup = Startup::new(config).unwrap();
        assert_eq!(
            startup.push(
                frame(config, 0, now + Duration::from_millis(50)),
                quantum,
                now
            ),
            Err(Error::Configuration)
        );
        assert!(startup.is_empty());
    }
}

#[test]
fn expiry_is_rechecked_before_and_after_every_batch_admission() {
    let now = Instant::now();
    for expire_after in 0..=3 {
        let config = configuration(10, AudioChannels::Mono);
        let mut startup = Startup::new(config).unwrap();
        let until = now + Duration::from_millis(5);
        for index in 0..3_u64 {
            let expires = if index == 0 {
                until
            } else {
                now + Duration::from_millis(50)
            };
            startup
                .push(frame(config, 1_000_000 + index * 480, expires), 1024, now)
                .unwrap();
        }
        let batch = startup.take(1024, 4, now).unwrap().unwrap();
        let clock = Cell::new(if expire_after == 0 { until } else { now });
        let mut calls = 0;
        let result = batch.admit::<Error>(
            10_000,
            || clock.get(),
            |_, _| {
                calls += 1;
                if calls == expire_after {
                    clock.set(until);
                }
                Ok(())
            },
        );
        assert_eq!(result, Err(Error::Expired));
        assert_eq!(calls, expire_after);
        assert!(startup.is_empty());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Policy(Error),
    Admission,
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Policy(error)
    }
}

#[test]
fn partial_admission_failure_drops_remainder_without_another_callback_or_retry() {
    let now = Instant::now();
    for fail_at in 0..3 {
        let mut startup = complete(1024, now);
        let batch = startup.take(1024, 4, now).unwrap().unwrap();
        let mut calls = 0;
        let result = batch.admit::<Failure>(
            10_000,
            || now,
            |frame, start| {
                let ordinal = calls;
                calls += 1;
                assert_eq!(start, 10_000 + u64::try_from(ordinal).unwrap() * 480);
                assert_eq!(
                    frame.pcm.0.timestamp_samples(),
                    1_000_000 + u64::try_from(ordinal).unwrap() * 480
                );
                if ordinal == fail_at {
                    Err(Failure::Admission)
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result, Err(Failure::Admission));
        assert_eq!(calls, fail_at + 1);
        assert!(startup.is_empty());
        assert!(matches!(startup.take(1024, 4, now), Err(Error::Metadata)));
    }
}

#[test]
fn source_and_absolute_output_overflow_refuse_before_publication() {
    let now = Instant::now();
    let config = configuration(10, AudioChannels::Mono);
    let mut startup = Startup::new(config).unwrap();
    assert_eq!(
        startup.push(
            frame(config, u64::MAX - 100, now + Duration::from_millis(50)),
            512,
            now
        ),
        Err(Error::Timeline)
    );
    assert!(startup.is_empty());
    let mut startup = complete(512, now);
    let batch = startup.take(512, 4, now).unwrap().unwrap();
    let result = batch.admit::<Error>(
        u64::MAX - 100,
        || now,
        |_, _| panic!("overflow cannot admit any original PCM"),
    );
    assert_eq!(result, Err(Error::Timeline));
}
