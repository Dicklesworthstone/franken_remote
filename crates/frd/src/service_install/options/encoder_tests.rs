//! The rendered service must select the SAME encoder when parsed by frd run.
//! These are configuration tests, not service activation or GPU qualification.
use super::*;
use crate::{
    host_policy::{Approval, Sharing, options::RunOptions},
    host_run::Encoder,
    service_install::render_systemd_unit,
};

fn parse(args: &[&str]) -> Result<InstallOptions, ServiceError> {
    InstallOptions::parse_cli(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
}
fn emitted(options: &InstallOptions) -> RunOptions {
    // Escaping has separate tests. Keep every path in this roundtrip fixture
    // free of whitespace so it can be split without inventing a systemd parser.
    let mut options = options.clone();
    options.exec_path = PathBuf::from("/usr/libexec/frd");
    let unit = render_systemd_unit(&options);
    let line = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart=:"))
        .unwrap();
    let args = line.split_whitespace().skip(2).map(str::to_owned).collect::<Vec<_>>();
    RunOptions::parse(&args).unwrap()
}

#[test]
fn encoder_selection_survives_the_rendered_systemd_command_without_fallback() {
    for (name, encoder) in [
        ("nvenc", Encoder::Nvenc),
        ("vaapi", Encoder::Vaapi),
        ("software", Encoder::SoftwareExplicit),
    ] {
        let options = parse(&[
            "--user", "--dry-run", "--approval", "none", "--sharing", "own-user",
            "--", "--encoder", name, "--worker", "/usr/libexec/fr-media-worker",
            "--display", ":0",
        ]).unwrap();
        assert!(!options.software_explicit);
        assert!(!render_systemd_unit(&options).contains("--software-explicit"));
        let run = emitted(&options);
        assert_eq!(run.selected_encoder().unwrap(), Some(encoder));
        assert_eq!(run.approval, Some(Approval::None));
        assert_eq!(run.sharing, Some(Sharing::OwnUser));
        assert_eq!(run.worker, Some(PathBuf::from("/usr/libexec/fr-media-worker")));
        assert_eq!(run.display.as_deref(), Some(":0"));
        assert!(run.input_agent.is_none() && !run.audio && !run.clipboard && run.files.is_none());
    }
}

#[test]
fn legacy_software_and_explicit_backend_cannot_both_enter_a_unit() {
    for name in ["software", "nvenc", "vaapi"] {
        let conflicting = parse(&[
            "--user", "--software-explicit", "--dry-run", "--", "--encoder", name,
        ]);
        assert!(matches!(conflicting, Err(ServiceError::InvalidOptions)));
        let mut programmatic = InstallOptions {
            kind: ServiceKind::SystemdUser,
            software_explicit: true,
            run_args: vec!["--encoder".into(), name.into()],
            ..InstallOptions::default()
        };
        assert_eq!(programmatic.validate(), Err(ServiceError::InvalidOptions));
        programmatic.software_explicit = false;
        assert!(programmatic.validate().is_ok());
    }
    let legacy = parse(&["--user", "--software-explicit", "--dry-run"]).unwrap();
    assert_eq!(emitted(&legacy).selected_encoder().unwrap(), Some(Encoder::SoftwareExplicit));
}

#[test]
fn malformed_or_duplicate_encoder_flags_are_refused_before_installation() {
    for tail in [
        vec!["--encoder"],
        vec!["--encoder", "auto"],
        vec!["--encoder", "libx265"],
        vec!["--encoder", "nvenc", "--encoder", "vaapi"],
        vec!["--encoder", "software", "--software-explicit"],
        vec!["--encoder", "nvenc", "--once"],
        vec!["--encoder", "nvenc", "--approval", "none"],
    ] {
        let args = ["--user", "--dry-run", "--"]
            .into_iter().chain(tail.iter().copied()).collect::<Vec<_>>();
        assert!(matches!(parse(&args), Err(ServiceError::InvalidOptions)), "{tail:?}");
    }
    assert!(matches!(
        parse(&["--user", "--dry-run"]),
        Err(ServiceError::HostProfileUnavailable { code: "hardware_hevc_unavailable", .. })
    ));
}

#[test]
fn encoder_selection_preserves_authority_policy_and_other_explicit_features() {
    let options = parse(&[
        "--user", "--dry-run", "--", "--encoder", "vaapi",
        "--input-agent", "/input", "--clipboard", "--audio", "--files", "/drop",
        "--logind-session", "c2",
    ]).unwrap();
    assert!(options.approval_mode.is_empty() && options.sharing_scope.is_empty());
    let run = emitted(&options);
    assert_eq!(run.selected_encoder().unwrap(), Some(Encoder::Vaapi));
    assert!(run.approval.is_none() && run.sharing.is_none());
    assert_eq!(run.input_agent, Some(PathBuf::from("/input")));
    assert!(run.clipboard && run.audio);
    assert_eq!(run.files, Some(PathBuf::from("/drop")));
    assert_eq!(run.logind_session.as_deref(), Some("c2"));
    assert!(matches!(
        parse(&["--user", "--dry-run", "--approval", "local", "--", "--encoder", "nvenc"]),
        Err(ServiceError::HostProfileUnavailable { code: "local_approval_unavailable", .. })
    ));
}

#[test]
fn hardware_selection_cannot_bypass_system_service_or_platform_restrictions() {
    assert!(matches!(
        parse(&["--system", "--dry-run", "--", "--encoder", "nvenc"]),
        Err(ServiceError::HostProfileUnavailable { code: "system_service_requires_headless", .. })
    ));
    let options = parse(&[
        "--system", "--dry-run", "--", "--encoder", "nvenc", "--headless",
    ]).unwrap();
    let run = emitted(&options);
    assert!(run.headless);
    assert_eq!(run.selected_encoder().unwrap(), Some(Encoder::Nvenc));
    for kind in [ServiceKind::LaunchdAgent, ServiceKind::WindowsService] {
        let options = InstallOptions { kind, ..options.clone() };
        assert!(matches!(options.validate(), Err(ServiceError::UnsupportedPlatform { .. })));
    }
}

#[test]
fn a_hardware_view_only_service_retains_the_validated_observation_indicator() {
    let options = parse(&[
        "--user", "--dry-run", "--", "--encoder", "vaapi",
        "--observation-indicator", "/usr/libexec/fr-observation-indicator",
    ]).unwrap();
    let run = emitted(&options);
    assert_eq!(run.selected_encoder().unwrap(), Some(Encoder::Vaapi));
    assert_eq!(run.observation_indicator, Some(PathBuf::from("/usr/libexec/fr-observation-indicator")));
    for path in ["relative", "/tmp/../indicator", "/tmp/indicator\nOther=thing"] {
        assert!(matches!(
            parse(&["--user", "--dry-run", "--", "--encoder", "vaapi", "--observation-indicator", path]),
            Err(ServiceError::InvalidOptions)
        ));
    }
}
