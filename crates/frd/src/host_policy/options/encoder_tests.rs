//! Exact CLI selection, without filesystem, codec, network or input work.
use super::*;

fn parse(args: &[&str]) -> Result<RunOptions, Error> {
    RunOptions::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn encoder_selection_is_explicit_and_keeps_the_software_alias() {
    assert_eq!(parse(&[]).unwrap().selected_encoder().unwrap(), None);
    assert_eq!(
        parse(&["--software-explicit"])
            .unwrap()
            .selected_encoder()
            .unwrap(),
        Some(Encoder::SoftwareExplicit)
    );
    for (name, expected) in [
        ("software", Encoder::SoftwareExplicit),
        ("nvenc", Encoder::Nvenc),
        ("vaapi", Encoder::Vaapi),
    ] {
        let options = parse(&["--encoder", name]).unwrap();
        assert_eq!(options.selected_encoder().unwrap(), Some(expected));
        assert!(!options.software_explicit);
    }
}

#[test]
fn encoder_selection_rejects_ambiguity_in_both_argument_orders() {
    for encoder in ["software", "nvenc", "vaapi"] {
        for args in [
            vec!["--encoder", encoder, "--software-explicit"],
            vec!["--software-explicit", "--encoder", encoder],
            vec!["--encoder", encoder, "--encoder", encoder],
            vec!["--encoder", encoder, "--encoder", "software"],
        ] {
            assert!(matches!(parse(&args), Err(Error::InvalidArgument)), "{args:?}");
        }
    }
}

#[test]
fn encoder_selection_does_not_accept_automatic_or_arbitrary_codec_names() {
    for args in [
        vec!["--encoder"],
        vec!["--encoder", "--once"],
        vec!["--encoder", ""],
        vec!["--encoder", "auto"],
        vec!["--encoder", "h264"],
        vec!["--encoder", "hevc_nvenc"],
        vec!["--encoder", "libx265"],
        vec!["--encoder", "videotoolbox"],
        vec!["--encoder", "NVENC"],
        vec!["--encoder", "vaapi "],
        vec!["--encoder=nvenc"],
    ] {
        assert!(matches!(parse(&args), Err(Error::InvalidArgument)), "{args:?}");
    }
}

#[test]
fn hardware_selection_never_enables_control_audio_or_disables_approval() {
    for encoder in ["nvenc", "vaapi"] {
        let options = parse(&["--encoder", encoder]).unwrap();
        assert!(options.input_agent.is_none());
        assert!(!options.audio && !options.clipboard);
        assert!(options.files.is_none());
        assert_eq!(options.approval, None);
        assert_eq!(options.sharing, None);
        let local = parse(&["--encoder", encoder, "--approval", "local"]).unwrap();
        assert_eq!(local.approval, Some(Approval::Local));
        assert_eq!(local.selected_encoder().unwrap(), Encoder::parse(encoder));
    }
}

#[test]
fn encoder_choice_is_orthogonal_to_other_explicit_features() {
    let controlled = parse(&[
        "--encoder", "nvenc", "--input-agent", "/input", "--clipboard", "--audio",
        "--files", "/drop", "--logind-session", "c2",
    ]).unwrap();
    assert_eq!(controlled.selected_encoder().unwrap(), Some(Encoder::Nvenc));
    assert!(controlled.clipboard && controlled.audio);
    assert_eq!(controlled.input_agent, Some(PathBuf::from("/input")));
    assert_eq!(controlled.files, Some(PathBuf::from("/drop")));
    assert_eq!(controlled.logind_session.as_deref(), Some("c2"));
    let observed = parse(&[
        "--encoder", "vaapi", "--observation-indicator", "/indicator", "--audio",
    ]).unwrap();
    assert_eq!(observed.selected_encoder().unwrap(), Some(Encoder::Vaapi));
    assert_eq!(observed.observation_indicator, Some(PathBuf::from("/indicator")));
    assert!(observed.audio && observed.input_agent.is_none());
}

#[test]
fn programmatic_conflicts_fail_before_loading_saved_policy() {
    for encoder in [Encoder::SoftwareExplicit, Encoder::Nvenc, Encoder::Vaapi] {
        let options = RunOptions {
            software_explicit: true,
            encoder: Some(encoder),
            // A relative policy path would fail differently if resolution
            // reached the filesystem/configuration path first.
            config: Some(PathBuf::from("relative-policy")),
            ..RunOptions::default()
        };
        assert!(matches!(options.selected_encoder(), Err(Error::InvalidArgument)));
        assert!(matches!(options.resolve(), Err(Error::InvalidArgument)));
    }
}
