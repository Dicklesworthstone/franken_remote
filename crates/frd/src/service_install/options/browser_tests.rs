use super::*;
use crate::{
    host_policy::{Approval, options::RunOptions},
    service_install::{probe_port, render_systemd_unit},
};
use std::net::{TcpListener, UdpSocket};

fn options() -> InstallOptions {
    InstallOptions {
        kind: ServiceKind::SystemdUser,
        listener: ServiceListener::Browser,
        service_port: 9444,
        exec_path: PathBuf::from("/usr/libexec/frd"),
        dry_run: true,
        run_args: [
            "--encoder",
            "auto",
            "--display",
            ":0",
            "--logind-session",
            "c2",
            "--session-monitor",
            "/usr/libexec/fr-session-monitor",
            "--observation-indicator",
            "/usr/libexec/fr-observation-indicator",
        ]
        .map(str::to_owned)
        .into(),
        ..InstallOptions::default()
    }
}

fn emitted(options: &InstallOptions) -> RunOptions {
    let unit = render_systemd_unit(options);
    let command = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart=:"))
        .unwrap();
    // Fixture values contain no whitespace. The real renderer's argument
    // escaping is exercised independently by the existing quoting tests.
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
fn one_browser_listener_and_its_original_desktop_survive_service_rendering() {
    for mode in ["", "none", "local"] {
        let mut options = options();
        options.approval_mode = mode.into();
        options.sharing_scope = "own-user".into();
        let run = emitted(&options);
        assert_eq!(run.browser_port, Some(9444));
        assert_eq!(run.port, None);
        assert_eq!(run.logind_session.as_deref(), Some("c2"));
        assert_eq!(run.display.as_deref(), Some(":0"));
        assert_eq!(
            run.observation_indicator.as_deref(),
            Some(Path::new("/usr/libexec/fr-observation-indicator"))
        );
        assert_eq!(
            run.approval,
            match mode {
                "local" => Some(Approval::Local),
                "none" => Some(Approval::None),
                _ => None,
            }
        );
        assert_eq!(run.selected_encoder().unwrap().unwrap().as_str(), "auto");
        assert!(!run.once && !run.control && !run.audio && !run.clipboard);
        // The availability argument is explicit test evidence for profile
        // validation, not a fabricated embedded bundle or running service.
        assert_eq!(options.validate_browser_run(&run, true), Ok(()));
        if mode == "local" {
            options.validate_local_approval().unwrap();
        }
        let unit = render_systemd_unit(&options);
        assert!(unit.contains("run --browser-port 9444"));
        assert!(unit.contains("PartOf=graphical-session.target"));
        assert!(unit.contains("RestartPreventExitStatus=2"));
    }
}

#[test]
fn browser_service_requires_the_real_builds_embedded_bundle() {
    let options = options();
    let run = emitted(&options);
    assert!(matches!(
        options.validate_browser_run(&run, false),
        Err(ServiceError::HostProfileUnavailable {
            code: "browser_client_unavailable",
            ..
        })
    ));
    let mut args = [
        "--user",
        "--dry-run",
        "--browser-port",
        "9444",
        "--approval",
        "local",
        "--",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(options.run_args.clone());
    let parsed = InstallOptions::parse_cli(&args);
    let result = options.validate();
    if crate::broker::browser_assets::BrowserAssets::bundled() {
        result.unwrap();
        let parsed = parsed.unwrap();
        assert_eq!(parsed.listener, ServiceListener::Browser);
        assert_eq!(parsed.service_port, 9444);
        assert_eq!(emitted(&parsed).browser_port, Some(9444));
    } else {
        assert!(matches!(
            result,
            Err(ServiceError::HostProfileUnavailable {
                code: "browser_client_unavailable",
                ..
            })
        ));
        assert!(matches!(
            parsed,
            Err(ServiceError::HostProfileUnavailable {
                code: "browser_client_unavailable",
                ..
            })
        ));
    }
}

#[test]
fn native_and_browser_port_selections_cannot_compete_or_be_smuggled() {
    for args in [
        vec!["--port", "8443", "--browser-port", "9444"],
        vec!["--browser-port", "9444", "--port", "8443"],
        vec!["--browser-port", "9444", "--browser-port", "9445"],
        vec!["--browser-port", "0"],
        vec!["--browser-port", "65536"],
        vec!["--browser-port"],
    ] {
        assert!(matches!(
            InstallOptions::parse_cli(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
            Err(ServiceError::InvalidOptions)
        ));
    }
    for listener in [ServiceListener::Native, ServiceListener::Browser] {
        let mut options = options();
        options.listener = listener;
        options
            .run_args
            .extend(["--browser-port".into(), "8445".into()]);
        assert_eq!(options.validate(), Err(ServiceError::InvalidOptions));
    }
}

#[test]
fn browser_service_refuses_missing_lifecycle_and_unsupported_effects() {
    let options = options();
    let mut missing_session = emitted(&options);
    missing_session.logind_session = None;
    let mut missing_indicator = emitted(&options);
    missing_indicator.observation_indicator = None;
    let mut input = emitted(&options);
    input.input_agent = Some(PathBuf::from("/usr/libexec/fr-input-agent"));
    let mut audio = emitted(&options);
    audio.audio = true;
    for (profile, expected) in [
        (missing_session, "browser_session_monitor_required"),
        (missing_indicator, "browser_indicator_required"),
        (input, "browser_feature_unsupported"),
        (audio, "browser_feature_unsupported"),
    ] {
        assert!(matches!(
            options.validate_browser_run(&profile, true),
            Err(ServiceError::HostProfileUnavailable { code, .. }) if code == expected
        ));
    }
}

#[test]
fn browser_host_is_not_installed_outside_its_selected_user_session() {
    for kind in [ServiceKind::SystemdSystem, ServiceKind::LaunchdAgent] {
        let mut options = options();
        options.kind = kind;
        assert!(matches!(
            options.validate(),
            Err(ServiceError::HostProfileUnavailable {
                code: "browser_host_unsupported",
                ..
            })
        ));
    }
}

#[test]
fn port_preflight_checks_tcp_for_browser_and_udp_for_native() {
    let tcp = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let tcp_port = tcp.local_addr().unwrap().port();
    assert_eq!(
        probe_port(ServiceListener::Browser, tcp_port)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    // Hold the other protocol too, making the independence assertion
    // deterministic without assuming the test host has no other listeners.
    if let Ok(udp) = UdpSocket::bind(("127.0.0.1", tcp_port)) {
        drop(udp);
        assert!(probe_port(ServiceListener::Native, tcp_port).is_ok());
    }
    let udp = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    assert_eq!(
        probe_port(ServiceListener::Native, udp_port)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    if let Ok(tcp) = TcpListener::bind(("127.0.0.1", udp_port)) {
        drop(tcp);
        assert!(probe_port(ServiceListener::Browser, udp_port).is_ok());
    }
}
