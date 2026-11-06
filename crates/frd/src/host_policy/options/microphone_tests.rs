use super::{Error, RunOptions};

fn parse(arguments: &str) -> Result<RunOptions, Error> {
    RunOptions::parse(
        &arguments
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}

#[test]
fn microphone_requires_a_separate_explicit_local_enable_and_controller() {
    let ordinary = parse("--input-agent /fixture/fr-input-agent --audio").unwrap();
    assert!(
        !ordinary.microphone,
        "playback/input consent cannot enable microphone"
    );
    let enabled = parse("--input-agent /fixture/fr-input-agent --microphone --microphone-server /fixture/pipewire-0").unwrap();
    assert!(enabled.microphone);
    assert!(
        !enabled.audio,
        "microphone approval cannot enable playback capture"
    );
    assert_eq!(
        enabled.microphone_server.as_deref(),
        Some(std::path::Path::new("/fixture/pipewire-0"))
    );
}

#[test]
fn microphone_typos_or_missing_control_refuse_without_endpoint_access() {
    for arguments in [
        "--microphone",
        "--microphone-server /fixture/pipewire-0",
        "--input-agent /fixture/input --microphone-server /fixture/pipewire-0",
        "--input-agent /fixture/input --microphone --microphone",
        "--input-agent /fixture/input --microphone --microphone-server",
        "--input-agent /fixture/input --microphone --microphone-server relative",
        "--input-agent /fixture/input --microphone --microphone-server /a --microphone-server /b",
    ] {
        assert!(
            matches!(parse(arguments), Err(Error::InvalidArgument)),
            "{arguments}"
        );
    }
}
