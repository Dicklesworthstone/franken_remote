//! Real rate parsing and rendered service arguments; no native/GPU evidence.
use super::*;
use crate::service_install::{InstallOptions, ServiceKind, render_systemd_unit};

fn parse(args: &[&str]) -> Result<RunOptions, Error> {
    RunOptions::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn video_defaults_and_individual_overrides_are_independent() {
    let default = parse(&[]).unwrap();
    assert!(default.fps.is_none() && default.bitrate.is_none());
    assert_eq!(default.video_profile().unwrap(), video::Profile::default());
    assert_eq!(default.selected_encoder().unwrap(), None);
    let rate = parse(&["--fps", "60"]).unwrap().video_profile().unwrap();
    assert_eq!(rate.fps(), 60);
    assert_eq!(rate.bitrate(), video::DEFAULT_BITRATE);
    let bitrate = parse(&["--bitrate", "2000000"]).unwrap().video_profile().unwrap();
    assert_eq!(bitrate.fps(), video::DEFAULT_FPS);
    assert_eq!(bitrate.bitrate(), 2_000_000);
}

#[test]
fn video_limits_are_enforced_without_rounding_clamping_or_implicit_units() {
    for (fps, bitrate) in [("1", "10000"), ("60", "12000000"), ("240", "200000000")] {
        let options = parse(&["--fps", fps, "--bitrate", bitrate]).unwrap();
        let profile = options.video_profile().unwrap();
        assert_eq!(profile.fps().to_string(), fps);
        assert_eq!(profile.bitrate().to_string(), bitrate);
    }
    for (flag, values) in [
        ("--fps", &["0", "241", "65536", "60fps", "59.94", "1e2", " 60", "+60", "-1"][..]),
        ("--bitrate", &["0", "9999", "200000001", "4294967296", "8M", "8_000_000", "8.0", "+8000000", "8000000 "][..]),
    ] {
        for value in values {
            assert!(matches!(parse(&[flag, value]), Err(Error::InvalidArgument)), "{flag} {value}");
        }
    }
}

#[test]
fn video_duplicate_and_valueless_flags_refuse_in_either_order() {
    for (flag, first, second) in [("--fps", "30", "60"), ("--bitrate", "10000", "12000000")] {
        for args in [
            vec![flag],
            vec![flag, ""],
            vec![flag, "--once"],
            vec![flag, first, flag, first],
            vec![flag, first, flag, second],
            vec![flag, second, flag, first],
        ] {
            assert!(matches!(parse(&args), Err(Error::InvalidArgument)), "{args:?}");
        }
    }
}

#[test]
fn video_options_do_not_select_a_codec_or_enable_authority_and_optional_lanes() {
    for encoder in ["software", "nvenc", "vaapi"] {
        let options = parse(&["--encoder", encoder, "--fps", "60", "--bitrate", "12000000"]).unwrap();
        assert_eq!(options.selected_encoder().unwrap(), Encoder::parse(encoder));
        assert_eq!(options.video_profile().unwrap(), video::Profile::new(60, 12_000_000).unwrap());
        assert!(options.approval.is_none() && options.sharing.is_none());
        assert!(!options.audio && !options.clipboard && options.files.is_none());
        assert!(options.input_agent.is_none() && options.logind_session.is_none());
        let local = parse(&["--encoder", encoder, "--fps", "60", "--approval", "local"]).unwrap();
        assert_eq!(local.approval, Some(Approval::Local));
    }
    assert_eq!(parse(&["--fps", "60", "--bitrate", "12000000"]).unwrap().selected_encoder().unwrap(), None);
    assert!(parse(&["--encoder", "nvenc", "--software-explicit", "--fps", "60"]).is_err());
}

#[test]
fn video_programmatic_invalid_options_refuse_before_policy_resolution() {
    for (fps, bitrate) in [(0, 8_000_000), (241, 8_000_000), (30, 9_999), (30, 200_000_001)] {
        let options = RunOptions {
            // Were policy resolution reached, this is an InvalidPath, not an
            // InvalidArgument. No filesystem or display call is needed here.
            config: Some(PathBuf::from("relative/policy.json")),
            fps: Some(fps),
            bitrate: Some(bitrate),
            ..RunOptions::default()
        };
        assert_eq!(options.video_profile(), Err(Error::InvalidArgument));
        assert!(matches!(options.resolve(), Err(Error::InvalidArgument)));
    }
}

#[test]
fn video_rate_targets_survive_systemd_render_and_the_actual_host_parser() {
    for encoder in ["software", "nvenc", "vaapi"] {
        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            exec_path: PathBuf::from("/usr/bin/frd"),
            approval_mode: "none".into(),
            run_args: ["--encoder", encoder, "--fps", "60", "--bitrate", "12000000"]
                .into_iter().map(str::to_owned).collect(),
            ..InstallOptions::default()
        };
        options.validate().unwrap();
        let unit = render_systemd_unit(&options);
        let command = unit.lines().find_map(|line| line.strip_prefix("ExecStart=:")).unwrap();
        let args = command.split_whitespace().skip(2).map(str::to_owned).collect::<Vec<_>>();
        let parsed = RunOptions::parse(&args).unwrap();
        assert_eq!(parsed.video_profile().unwrap(), video::Profile::new(60, 12_000_000).unwrap());
        assert_eq!(parsed.selected_encoder().unwrap(), Encoder::parse(encoder));
        assert_eq!(parsed.approval, Some(Approval::None));
        assert!(!command.contains("--software-explicit"));
        assert_eq!(command.matches("--fps").count(), 1);
        assert_eq!(command.matches("--bitrate").count(), 1);
    }
}

#[test]
fn video_rates_in_service_args_retain_legacy_software_and_existing_restrictions() {
    let mut options = InstallOptions {
        kind: ServiceKind::SystemdUser,
        exec_path: PathBuf::from("/usr/bin/frd"),
        software_explicit: true,
        run_args: ["--fps", "15", "--bitrate", "2000000"].into_iter().map(str::to_owned).collect(),
        ..InstallOptions::default()
    };
    options.validate().unwrap();
    assert!(render_systemd_unit(&options).contains("--software-explicit --fps 15 --bitrate 2000000"));
    options.run_args.extend(["--fps", "30"].into_iter().map(str::to_owned));
    assert!(options.validate().is_err());
    options.run_args = ["--fps", "241"].into_iter().map(str::to_owned).collect();
    assert!(options.validate().is_err());
    options.run_args = ["--fps", "60"].into_iter().map(str::to_owned).collect();
    options.approval_mode = "local".into();
    assert!(options.validate().is_err());
    options.approval_mode.clear();
    options.kind = ServiceKind::LaunchdAgent;
    assert!(options.validate().is_err());
}
