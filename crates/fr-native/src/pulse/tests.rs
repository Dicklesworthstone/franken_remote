use super::output::validate_submission_clock;
use super::*;
use fr_core::audio::{AudioChannels, AudioGeneration};
#[test]
fn selection_never_accepts_remote_servers_default_aliases_or_lists() {
    for path in [
        "tcp:host",
        "relative",
        "/tmp/two servers",
        "/tmp/a\n",
        "/tmp/:server",
    ] {
        assert!(Selection::new(Path::new(path), "output").is_err());
    }
    for sink in ["", "@DEFAULT_SINK@", "name;other", "path/name", "newline\n"] {
        assert!(Selection::new(Path::new("/tmp/pulse/native"), sink).is_err());
    }
    let selected = Selection::new(
        Path::new("/run/user/1000/pulse/native"),
        "alsa_output.card-1",
    )
    .unwrap();
    assert!(!format!("{selected:?}").contains("1000"));
}
fn submission() -> AudioSubmission {
    AudioSubmission {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::INITIAL,
        sequence: 0,
        source_samples: 500_000,
        output_samples: 100,
        valid_until: ClientInstant(100_000),
        output_valid_before: 580,
        concealed: false,
    }
}
#[test]
fn device_latency_must_fit_original_packet_deadline_and_slot() {
    let audio = submission();
    let end = audio.output_valid_before + LEAD_SAMPLES;
    assert_eq!(
        validate_submission_clock(
            PlayoutClock {
                now: ClientInstant(1_000),
                output_samples: 100
            },
            audio,
            end
        ),
        Ok(())
    );
    assert_eq!(
        validate_submission_clock(
            PlayoutClock {
                now: ClientInstant(80_000),
                output_samples: 100
            },
            audio,
            end
        ),
        Err(Error::Expired)
    );
    assert_eq!(
        validate_submission_clock(
            PlayoutClock {
                now: ClientInstant(1_000),
                output_samples: 580
            },
            audio,
            end
        ),
        Err(Error::Clock)
    );
    assert_eq!(
        validate_submission_clock(
            PlayoutClock {
                now: ClientInstant(1_000),
                output_samples: 99
            },
            audio,
            end
        ),
        Err(Error::Clock)
    );
}
#[test]
fn microphone_and_overlong_output_profiles_refuse_before_native_connect() {
    for (direction, duration) in [(AudioDirection::Uplink, 10), (AudioDirection::Downlink, 40)] {
        let selected = Selection::new(Path::new("/tmp/nonexistent-fr-pulse"), "output").unwrap();
        let config = AudioStreamConfig::new(
            direction,
            AudioGeneration::INITIAL,
            AudioChannels::Stereo,
            duration,
            20,
        )
        .unwrap();
        assert!(matches!(
            PlaybackDevice::connect(selected, config, ClientInstant(0)),
            Err(Error::Configuration)
        ));
    }
}
