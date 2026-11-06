use super::*;

fn connection(extra: &str) -> Result<Connection, Failure> {
    let words = format!(
        "connect n-host --experimental-native --display only --worker /installed/fr-media-worker {extra}"
    );
    let options = parse(
        &words
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )?;
    let Command::Connect(connection) = options.command else {
        panic!("connect command")
    };
    Ok(connection)
}

#[test]
fn microphone_is_separate_from_control_playback_and_connecting() {
    for extra in ["--control", "--control --audio", "--view-only --audio"] {
        assert!(connection(extra).unwrap().microphone.is_none());
    }
    let chosen = connection("--control --microphone --microphone-source alsa_input.usb-device.analog-mono --microphone-server /run/user/7/pulse/native").unwrap();
    assert!(chosen.audio.is_none());
    assert_eq!(
        chosen.microphone,
        Some(MicrophoneRequest {
            server: Some(PathBuf::from("/run/user/7/pulse/native")),
            source: "alsa_input.usb-device.analog-mono".into(),
        })
    );
    assert!(chosen.control && chosen.clipboard);
}

#[test]
fn microphone_requires_control_explicit_source_and_its_own_enable() {
    for extra in [
        "--view-only --microphone --microphone-source source",
        "--control --microphone",
        "--control --microphone-server /run/native",
        "--control --microphone-source source",
        "--control --microphone --microphone --microphone-source source",
        "--control --microphone --microphone-source source --microphone-source other",
        "--control --microphone --microphone-source @DEFAULT_SOURCE@",
        "--control --microphone --microphone-source remote:source",
        "--control --microphone --microphone-source source --microphone-server relative",
    ] {
        assert!(connection(extra).is_err(), "{extra}");
    }
    let huge = format!(
        "--control --microphone --microphone-source {}",
        "x".repeat(256)
    );
    assert!(connection(&huge).is_err());
    let bound = format!(
        "--control --microphone --microphone-source {}",
        "x".repeat(255)
    );
    assert!(connection(&bound).is_ok());
}

#[test]
fn microphone_cannot_enable_a_discovery_or_inspection_command() {
    for words in [
        "hosts --microphone --microphone-source source",
        "displays n-host --experimental-native --microphone --microphone-source source",
        "doctor --microphone",
        "inspect n-host --microphone",
    ] {
        assert!(
            parse(
                &words
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }
}
