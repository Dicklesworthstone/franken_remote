#![cfg(all(target_os = "linux", feature = "linux-pulse-playback"))]
//! Real local PulseAudio/null-output tests. Native API/server evidence, not
//! physical speakers, a live remote session, `PipeWire` or audible latency proof.
mod pulse_support;
use fr_client::{audio::playout::AudioSubmission, input::ClientInstant};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use fr_native::pulse::{
    Error, MAX_DEVICE_BUFFER_MS, PlaybackDevice, Selection, State, StopOutcome,
};
use pulse_support::{Server, absent_server_refuses};
use std::time::{Duration, Instant};

fn device(server: &Server) -> PlaybackDevice {
    let config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(11),
        AudioChannels::Stereo,
        10,
        20,
    )
    .unwrap();
    let mut device = PlaybackDevice::connect(
        Selection::new(&server.socket, "fr_test").unwrap(),
        config,
        server.now(),
    )
    .unwrap();
    server.ready(&mut device);
    device
}
fn frame(generation: AudioGeneration, sequence: u64) -> AudioPcmFrame {
    let data: Vec<i16> = (0..480)
        .flat_map(|i| {
            let value = (i16::try_from(i % 48).unwrap() - 24) * 300;
            [value, -value]
        })
        .collect();
    AudioPcmFrame::from_interleaved(
        generation,
        AudioChannels::Stereo,
        1_000_000 + sequence * 480,
        &data,
    )
    .unwrap()
}
fn receipt(
    device: &PlaybackDevice,
    sequence: u64,
    due: u64,
    now: ClientInstant,
) -> AudioSubmission {
    AudioSubmission {
        direction: AudioDirection::Downlink,
        generation: device.configuration().generation(),
        sequence,
        source_samples: 1_000_000 + sequence * 480,
        output_samples: due,
        output_valid_before: due + 480,
        valid_until: ClientInstant(now.0 + 100_000),
        concealed: false,
    }
}
#[test]
fn absent_native_server_fails_without_spawning_or_substituting_a_device() {
    for _ in 0..64 {
        assert!(absent_server_refuses().is_err());
    }
}
#[test]
fn real_server_negotiates_bounds_and_advances_clock_without_dummy_source_audio() {
    let server = Server::start();
    let mut device = device(&server);
    assert!(device.queue_capacity_bytes() > 0);
    assert!(device.queue_capacity_bytes() <= u32::from(MAX_DEVICE_BUFFER_MS) * 48 * 4);
    let first = device.clock(server.now()).unwrap();
    server.service_until(&mut device, first.output_samples + 480);
    assert!(device.clock(server.now()).unwrap().output_samples >= first.output_samples + 480);
}
#[test]
fn actual_pcm_reaches_an_independent_native_monitor_and_stop_flushes() {
    let server = Server::start();
    let mut device = device(&server);
    let capture = server.capture();
    // Keep the real native timing owner serviced while the independently-created
    // monitor starts. No fabricated counter or timer-derived output sample clock.
    device.poll(|| Ok(server.now())).unwrap();
    let first = device.clock(server.now()).unwrap().output_samples;
    for sequence in 0..24 {
        let due = first + sequence * 480;
        server.service_until(&mut device, due);
        let pcm = frame(device.configuration().generation(), sequence);
        let audio = receipt(&device, sequence, due, server.now());
        let result = device.submit(&pcm, audio, || Ok(server.now())).unwrap();
        assert_eq!(result.scheduled_output_sample, due + 960);
    }
    device.begin_stop(server.now()).unwrap();
    assert_eq!(device.state(), State::Stopping);
    let start = Instant::now();
    while device.state() != State::Closed {
        device.poll(|| Ok(server.now())).unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(device.stop_outcome(), Some(StopOutcome::Flushed));
    let pcm = capture.finish();
    assert_eq!(pcm.len(), 96_000);
    assert!(
        pcm.iter().filter(|s| s.unsigned_abs() > 500).count() > 5_000,
        "real output never reached the independent native monitor"
    );
    assert!(pcm.as_chunks::<2>().0.iter().filter(|s|i32::from(s[0]).abs()>500 && (i32::from(s[0])+i32::from(s[1])).abs()<5).count()>2_000,"stereo shape changed");
    // Last 200 ms is silent after acknowledged stream stop, not a queued replay.
    assert!(pcm[76_800..].iter().all(|s| s.unsigned_abs() < 5));
}
#[test]
fn stale_clock_and_expired_output_are_terminal_without_a_recovery_replay() {
    let server = Server::start();
    let mut expired = device(&server);
    let clock = expired.clock(server.now()).unwrap();
    let pcm = frame(expired.configuration().generation(), 0);
    let mut audio = receipt(&expired, 0, clock.output_samples, server.now());
    audio.valid_until = ClientInstant(server.now().0 + 5_000);
    assert_eq!(
        expired.submit(&pcm, audio, || Ok(server.now())),
        Err(Error::Expired)
    );
    assert_eq!(expired.state(), State::Closed);
    let mut stale = device(&server);
    std::thread::sleep(Duration::from_millis(55));
    assert_eq!(stale.clock(server.now()), Err(Error::Clock));
    assert_eq!(stale.state(), State::Closed);
}
#[test]
fn permission_changes_and_panics_fence_before_native_submission() {
    let server = Server::start();
    for panic_at in [None, Some(1), Some(2)] {
        let mut device = device(&server);
        let clock = device.clock(server.now()).unwrap();
        let pcm = frame(device.configuration().generation(), 0);
        let audio = receipt(&device, 0, clock.output_samples, server.now());
        let mut calls = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            device.submit(&pcm, audio, || {
                calls += 1;
                assert_ne!(
                    panic_at,
                    Some(calls),
                    "explicit local checkpoint panic fixture"
                );
                if panic_at.is_none() {
                    Err(Error::Denied)
                } else {
                    Ok(server.now())
                }
            })
        }));
        if panic_at.is_some() {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(Error::Denied));
        }
        assert_eq!(device.state(), State::Closed);
        assert_eq!(device.stop_outcome(), Some(StopOutcome::Disconnected));
    }
}
#[test]
fn losing_the_actual_server_retires_instead_of_reconnecting() {
    let mut server = Server::start();
    let mut device = device(&server);
    server.kill();
    let start = Instant::now();
    loop {
        if device.poll(|| Ok(server.now())).is_err() {
            break;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(device.state(), State::Closed);
    assert!(device.error().is_some());
}
