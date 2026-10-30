use super::super::{InstallOptions, ServiceError, ServiceKind, install, render_systemd_unit};
use crate::host_policy::{Approval, Sharing, options::RunOptions};
use std::path::PathBuf;

fn parse(args: &[&str]) -> Result<InstallOptions, ServiceError> {
    InstallOptions::parse_cli(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

fn emitted(options: &InstallOptions) -> RunOptions {
    let mut options = options.clone();
    options.exec_path = PathBuf::from("/usr/libexec/frd");
    let unit = render_systemd_unit(&options);
    // All values in this roundtrip fixture are single words. Quoting has its
    // own tests; do not invent a second systemd command parser here.
    let command = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart=:"))
        .unwrap();
    RunOptions::parse(
        &command
            .split_whitespace()
            .skip(2)
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn monitored_local_approval_survives_installation_and_host_argument_parsing() {
    for encoder in ["software", "nvenc", "vaapi"] {
        for clipboard in [None, Some(false), Some(true)] {
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
                "--display",
                ":0",
                "--logind-session",
                "c2",
                "--logind-seat",
                "seat0",
                "--session-monitor",
                "/usr/libexec/fr-session-monitor",
                "--worker",
                "/usr/libexec/fr-media-worker",
            ];
            if clipboard.is_some() {
                args.extend(["--input-agent", "/usr/libexec/fr-input-agent"]);
            }
            if clipboard == Some(false) {
                args.push("--no-clipboard");
            }
            let options = parse(&args).unwrap();
            let report = install(&options).unwrap();
            let run = emitted(&options);
            assert_eq!(run.approval, Some(Approval::Local));
            assert_eq!(run.sharing, Some(Sharing::OwnUser));
            assert_eq!(run.selected_encoder().unwrap().unwrap().as_str(), encoder);
            assert_eq!(run.logind_session.as_deref(), Some("c2"));
            assert_eq!(run.logind_seat.as_deref(), Some("seat0"));
            assert_eq!(
                run.session_monitor,
                Some(PathBuf::from("/usr/libexec/fr-session-monitor"))
            );
            assert_eq!(run.display.as_deref(), Some(":0"));
            assert_eq!(run.input_agent.is_some(), clipboard.is_some());
            assert_eq!(run.clipboard, clipboard.unwrap_or(false));
            assert!(!run.audio && run.files.is_none());
            assert!(!run.headless && run.observation_indicator.is_none());
            assert!(
                report
                    .unit_content
                    .contains("PartOf=graphical-session.target")
            );
            assert!(
                report
                    .unit_content
                    .contains("WantedBy=graphical-session.target")
            );
            assert!(report.unit_content.contains("RestartPreventExitStatus=2"));
            assert!(
                report
                    .next_steps
                    .iter()
                    .any(|step| step.contains("logind session"))
            );
        }
    }
}

#[test]
fn approval_installation_refuses_profiles_without_the_original_desktop_lifetime() {
    let base = [
        "--user",
        "--dry-run",
        "--software-explicit",
        "--approval",
        "local",
    ];
    for tail in [
        vec![],
        vec!["--", "--display", ":0"],
        vec!["--", "--logind-session", "c2", "--headless"],
        vec![
            "--",
            "--logind-session",
            "c2",
            "--observation-indicator",
            "/usr/libexec/fr-observation-indicator",
        ],
    ] {
        let args = base.iter().copied().chain(tail).collect::<Vec<_>>();
        assert!(
            matches!(
                parse(&args),
                Err(ServiceError::HostProfileUnavailable {
                    code: "local_approval_unavailable",
                    ..
                })
            ),
            "{args:?}"
        );
    }
    let system = parse(&[
        "--system",
        "--dry-run",
        "--software-explicit",
        "--approval",
        "local",
        "--",
        "--headless",
        "--logind-session",
        "c2",
    ]);
    assert!(matches!(
        system,
        Err(ServiceError::HostProfileUnavailable {
            code: "local_approval_unavailable",
            ..
        })
    ));
}

#[test]
fn local_approval_never_infers_a_session_or_overrides_saved_policy() {
    let inherited = parse(&[
        "--user",
        "--dry-run",
        "--software-explicit",
        "--config",
        "/private/policy.json",
        "--",
        "--logind-session",
        "c2",
    ])
    .unwrap();
    let run = emitted(&inherited);
    assert!(run.approval.is_none());
    assert!(run.sharing.is_none());
    assert_eq!(run.config, Some(PathBuf::from("/private/policy.json")));
    let explicit = parse(&[
        "--user",
        "--dry-run",
        "--software-explicit",
        "--approval",
        "local",
        "--",
        "--logind-session",
        "c2",
    ])
    .unwrap();
    assert_eq!(emitted(&explicit).approval, Some(Approval::Local));
    // Programmatic callers must satisfy the same constraints as the CLI.
    for options in [
        InstallOptions {
            kind: ServiceKind::SystemdSystem,
            ..explicit.clone()
        },
        InstallOptions {
            run_args: Vec::new(),
            ..explicit
        },
    ] {
        assert!(options.validate().is_err());
    }
}

#[test]
fn malformed_or_conflicting_session_arguments_never_generate_a_unit() {
    for tail in [
        vec!["--logind-session", "c2", "--logind-session", "c3"],
        vec!["--logind-session"],
        vec![
            "--logind-session",
            "c2",
            "--session-monitor",
            "relative-monitor",
        ],
        vec!["--logind-session", "c2", "--approval", "none"],
    ] {
        let args = [
            "--user",
            "--dry-run",
            "--software-explicit",
            "--approval",
            "local",
            "--",
        ]
        .into_iter()
        .chain(tail)
        .collect::<Vec<_>>();
        assert!(
            matches!(parse(&args), Err(ServiceError::InvalidOptions)),
            "{args:?}"
        );
    }
}
