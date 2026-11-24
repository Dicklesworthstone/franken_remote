//! Explicit Windows endpoint scope is separate from enabling playback or input.
use super::*;

fn parse(args: &[&str]) -> Result<RunOptions, Error> {
    RunOptions::parse(
        &args
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
    )
}

#[test]
fn playback_and_approval_never_imply_windows_endpoint_consent() {
    for approval in [Approval::None, Approval::Local] {
        let off = RunOptions::default();
        assert_eq!(off.windows_refusal(approval), None);
        let mut playback = RunOptions {
            audio: true,
            ..RunOptions::default()
        };
        assert_eq!(
            playback.windows_refusal(approval).map(|(code, _)| code),
            Some("audio_scope_not_authorized")
        );
        playback.audio_endpoint_wide = true;
        assert_eq!(playback.windows_refusal(approval), None);
        assert!(!playback.control && !playback.clipboard && !playback.microphone);
        assert!(playback.input_agent.is_none() && playback.files.is_none());
        playback.audio = false;
        assert_eq!(
            playback.windows_refusal(approval).map(|(code, _)| code),
            Some("audio_scope_requires_audio")
        );
    }
}

#[test]
fn windows_scope_does_not_reinterpret_linux_device_selectors() {
    for (server, sink) in [
        (Some(PathBuf::from("/run/pulse/native")), None),
        (None, Some("speakers".to_owned())),
    ] {
        let options = RunOptions {
            audio: true,
            audio_endpoint_wide: true,
            audio_server: server,
            audio_sink: sink,
            ..RunOptions::default()
        };
        for approval in [Approval::None, Approval::Local] {
            assert_eq!(
                options.windows_refusal(approval).map(|(code, _)| code),
                Some("audio_source_unsupported")
            );
        }
    }
}

#[test]
fn endpoint_acknowledgment_is_a_strict_independent_enable_choice() {
    for args in [
        &["--audio-endpoint-wide"][..],
        &["--audio", "--audio-endpoint-wide", "--audio-endpoint-wide"][..],
        &["--audio-endpoint-wide", "--audio", "--audio-endpoint-wide"][..],
        &["--audio", "--audio-endpoint-wide=true"][..],
        &["--audio", "--audio-endpoint-wide", "false"][..],
    ] {
        assert!(
            matches!(parse(args), Err(Error::InvalidArgument)),
            "{args:?}"
        );
    }
    // Both orders authorize only this launch on Windows. Linux and macOS
    // reject the flag, including alongside their otherwise valid --audio.
    for args in [
        &["--audio", "--audio-endpoint-wide"][..],
        &["--audio-endpoint-wide", "--audio"][..],
    ] {
        let selected = parse(args);
        if cfg!(windows) {
            let selected = selected.unwrap();
            assert!(selected.audio && selected.audio_endpoint_wide);
            assert_eq!(selected.windows_refusal(Approval::None), None);
        } else {
            assert!(matches!(selected, Err(Error::InvalidArgument)), "{args:?}");
        }
    }
    let ordinary = parse(&["--audio"]).unwrap();
    assert!(ordinary.audio && !ordinary.audio_endpoint_wide);
    assert!(!parse(&[]).unwrap().audio_endpoint_wide);
}

#[cfg(target_os = "macos")]
#[test]
fn constructed_windows_scope_never_falls_back_to_mac_system_audio() {
    let options = RunOptions {
        audio: true,
        audio_endpoint_wide: true,
        ..RunOptions::default()
    };
    assert_eq!(
        options.macos_refusal(Approval::Local).map(|(code, _)| code),
        Some("audio_source_unsupported")
    );
}
