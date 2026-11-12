use super::super::{InstallOptions, ServiceError, ServiceKind, render_launchd_plist};
use crate::host_policy::{Approval, Sharing, options::RunOptions};
use std::path::PathBuf;

fn parse(args: &[&str]) -> Result<InstallOptions, ServiceError> {
    InstallOptions::parse_cli(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

fn emitted(options: &InstallOptions) -> RunOptions {
    let plist = render_launchd_plist(options);
    // These fixture arguments contain no XML metacharacters. XML escaping
    // has its own tests; inspect only the original ProgramArguments array.
    let arguments = plist
        .split_once("<key>ProgramArguments</key>")
        .unwrap()
        .1
        .split_once("</array>")
        .unwrap()
        .0
        .lines()
        .filter_map(|line| line.trim().strip_prefix("<string>"))
        .filter_map(|line| line.strip_suffix("</string>"))
        .skip(2)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    RunOptions::parse(&arguments).unwrap()
}

#[test]
fn macos_local_approval_survives_installation_without_losing_stop_sharing() {
    for encoder in ["videotoolbox", "auto"] {
        for controlling in [false, true] {
            let mut args = vec![
                "--user",
                "--dry-run",
                "--approval",
                "local",
                "--sharing",
                "own-user",
                "--",
                "--encoder",
                encoder,
                "--session-monitor",
                "/local/fr-session-monitor",
                "--worker",
                "/local/fr-media-worker",
                "--observation-indicator",
                "/local/fr-observation-indicator",
            ];
            if controlling {
                args.extend(["--input-agent", "/local/fr-input-agent"]);
            }
            let mut options = parse(&args).unwrap();
            options.exec_path = PathBuf::from("/local/frd");
            assert_eq!(options.kind, ServiceKind::LaunchdAgent);
            let run = emitted(&options);
            assert_eq!(run.approval, Some(Approval::Local));
            assert_eq!(run.sharing, Some(Sharing::OwnUser));
            assert_eq!(run.selected_encoder().unwrap().unwrap().as_str(), encoder);
            assert_eq!(
                run.observation_indicator,
                Some(PathBuf::from("/local/fr-observation-indicator"))
            );
            assert_eq!(
                run.session_monitor,
                Some(PathBuf::from("/local/fr-session-monitor"))
            );
            assert_eq!(run.input_agent.is_some(), controlling);
            assert_eq!(run.clipboard, controlling);
            assert!(!run.audio && !run.microphone && run.files.is_none());
            assert!(run.logind_session.is_none() && run.display.is_none() && !run.headless);
            assert_eq!(run.macos_refusal(Approval::Local), None);
            let plist = render_launchd_plist(&options);
            assert!(plist.contains("<string>Aqua</string>"));
            assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        }
    }
}

#[test]
fn macos_approval_installer_keeps_original_profile_and_explicit_policy_boundaries() {
    let base = [
        "--user",
        "--dry-run",
        "--approval",
        "local",
        "--",
        "--encoder",
        "auto",
    ];
    for tail in [
        vec!["--headless"],
        vec!["--display", ":0"],
        vec!["--logind-session", "c2"],
        vec!["--approval", "none"],
        vec!["--observation-indicator", "relative-helper"],
    ] {
        let args = base.iter().copied().chain(tail).collect::<Vec<_>>();
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let mut explicit = parse(&base).unwrap();
    assert_eq!(emitted(&explicit).approval, Some(Approval::Local));
    assert!(emitted(&explicit).observation_indicator.is_none());
    // Missing override means the mandatory helper is resolved beside frd at
    // startup. Installation itself provides no native readiness or consent.
    explicit.kind = ServiceKind::LaunchdDaemon;
    assert!(matches!(
        explicit.validate(),
        Err(ServiceError::UnsupportedPlatform { .. })
    ));
    let inherited = parse(&["--user", "--dry-run", "--", "--encoder", "auto"]).unwrap();
    assert!(emitted(&inherited).approval.is_none());
    let none = parse(&[
        "--user",
        "--dry-run",
        "--approval",
        "none",
        "--",
        "--encoder",
        "auto",
    ])
    .unwrap();
    assert_eq!(emitted(&none).approval, Some(Approval::None));
}

#[test]
fn installed_playback_enable_is_preserved_without_inventing_a_pulse_source_or_microphone() {
    let base = [
        "--user",
        "--dry-run",
        "--approval",
        "local",
        "--",
        "--encoder",
        "videotoolbox",
    ];
    for enabled in [false, true] {
        let mut args = base.to_vec();
        if enabled {
            args.push("--audio");
        }
        let run = emitted(&parse(&args).unwrap());
        assert_eq!(run.audio, enabled);
        assert!(run.audio_server.is_none() && run.audio_sink.is_none());
        assert!(!run.microphone && run.input_agent.is_none());
        assert_eq!(run.approval, Some(Approval::Local));
        assert_eq!(run.macos_refusal(Approval::Local), None);
    }
    for tail in [
        ["--audio", "--audio-server", "/run/pulse/native"],
        ["--audio", "--audio-sink", "speakers"],
    ] {
        let args = base.iter().copied().chain(tail).collect::<Vec<_>>();
        assert!(matches!(
            parse(&args),
            Err(ServiceError::HostProfileUnavailable {
                code: "audio_source_unsupported",
                ..
            })
        ));
    }
}
