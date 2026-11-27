//! Browser CLI boundaries: requested native effects are refused before startup.
use super::*;
use std::path::PathBuf;

fn selected() -> RunOptions {
    RunOptions {
        browser_port: Some(8444),
        logind_session: Some("3".to_owned()),
        observation_indicator: Some(PathBuf::from("/usr/libexec/fr-observation-indicator")),
        ..RunOptions::default()
    }
}
fn code(options: &RunOptions, approval: Approval, bundled: bool) -> Option<&'static str> {
    options
        .browser_refusal(approval, bundled)
        .map(|(code, _)| code)
}

#[test]
fn browser_port_requires_one_nonzero_value() {
    for args in [
        vec!["--browser-port"],
        vec!["--browser-port", "0"],
        vec!["--browser-port", "65536"],
        vec!["--browser-port", "8444", "--browser-port", "8445"],
    ] {
        assert!(
            RunOptions::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err()
        );
    }
    let options = RunOptions::parse(&[
        "--browser-port".into(),
        "8444".into(),
        "--no-clipboard".into(),
    ])
    .unwrap();
    assert_eq!(options.browser_port, Some(8444));
    assert!(!options.clipboard);
}

#[test]
fn complete_view_only_profile_requires_no_implicit_approval_or_bundle() {
    let options = selected();
    assert_eq!(code(&options, Approval::None, true), None);
    assert_eq!(
        code(&options, Approval::Local, true),
        Some("browser_local_approval_unavailable"),
    );
    assert_eq!(
        code(&options, Approval::None, false),
        Some("browser_client_unavailable"),
    );
    assert_eq!(
        options.approval, None,
        "validation never changes saved or explicit policy"
    );
}

#[test]
fn login_and_visible_stop_are_required_independently() {
    let mut options = selected();
    options.logind_session = None;
    assert_eq!(
        code(&options, Approval::None, true),
        Some("browser_session_monitor_required"),
    );
    options.logind_session = Some("3".into());
    options.observation_indicator = None;
    assert_eq!(
        code(&options, Approval::None, true),
        Some("browser_indicator_required"),
    );
}

#[test]
fn every_unimplemented_effect_is_refused_instead_of_silently_ignored() {
    let changes: [fn(&mut RunOptions); 12] = [
        |o| o.input_agent = Some(PathBuf::from("/usr/libexec/fr-input-agent")),
        |o| o.control = true,
        |o| o.clipboard = true,
        |o| o.audio = true,
        |o| o.microphone = true,
        |o| o.files = Some(PathBuf::from("/private/drop")),
        |o| o.send.push(PathBuf::from("/private/file")),
        |o| o.send_control = Some("live".into()),
        |o| o.sync_source = Some(PathBuf::from("/private/source")),
        |o| o.sync_destination = Some(PathBuf::from("/private/destination")),
        |o| o.directory = true,
        |o| o.adaptive_capture = true,
    ];
    for change in changes {
        let mut options = selected();
        change(&mut options);
        assert_eq!(
            code(&options, Approval::None, true),
            Some("browser_feature_unsupported"),
        );
    }
}

#[test]
fn native_and_browser_listener_selectors_cannot_be_ambiguous() {
    let mut options = selected();
    options.port = Some(8443);
    assert_eq!(
        code(&options, Approval::None, true),
        Some("browser_profile_conflict"),
    );
}

#[test]
fn native_profile_keeps_its_existing_approval_and_feature_owners() {
    let mut options = selected();
    options.browser_port = None;
    options.control = true;
    options.audio = true;
    assert_eq!(code(&options, Approval::Local, false), None);
}
