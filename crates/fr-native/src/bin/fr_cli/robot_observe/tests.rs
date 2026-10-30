use super::*;

fn args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}
fn request(tail: &str) -> Result<Request, Failure> {
    parse(&args(&format!(
        "robot observe n-host --native-once --experimental-native --display only {tail}"
    )))
}
fn code(tail: &str) -> &'static str {
    request(tail).err().expect("must refuse").code
}

#[test]
fn explicit_observation_is_one_read_only_attempt_without_auxiliary_capabilities() {
    let request = request("--json").unwrap();
    assert!(request.json);
    assert_eq!(request.minimum, Minimum::Decoded);
    assert_eq!(request.connection.attempts, 1);
    assert!(!request.connection.control);
    assert!(!request.connection.clipboard);
    assert!(!request.connection.capture_system_shortcuts);
    assert!(request.connection.audio.is_none());
    assert!(request.connection.upload.paths.is_empty());
    assert!(!request.connection.upload.file_drop);
    assert!(request.connection.receive.is_none());
    assert_eq!(request.connection.display, options::DisplayChoice::Only);
}

#[test]
fn explicit_local_configuration_survives_translation() {
    let request = parse(&args(
        "robot observe host.fixture.ts.net --native-once --experimental-native --display 17 --by-name --port 9443 --socket /local/tailscaled.sock --trust-roots /local/roots.pem --worker /local/fr-media-worker --x-display :9 --ipv6 --fit 800x600 --evidence-level submitted_to_compositor --json",
    ))
    .unwrap();
    assert_eq!(request.connection.target.node, "host.fixture.ts.net");
    assert!(request.connection.target.by_name);
    assert!(request.connection.target.ipv6);
    assert_eq!(request.connection.target.port, 9443);
    assert_eq!(request.socket, Some(PathBuf::from("/local/tailscaled.sock")));
    assert_eq!(request.connection.target.roots, PathBuf::from("/local/roots.pem"));
    assert_eq!(request.connection.worker, PathBuf::from("/local/fr-media-worker"));
    assert_eq!(request.connection.x_display.as_deref(), Some(":9"));
    assert_eq!(request.connection.fit_window, Some((800, 600)));
    assert_eq!(request.connection.display, options::DisplayChoice::Handle(17));
    assert_eq!(request.minimum, Minimum::SubmittedToCompositor);
}

#[test]
fn no_control_auxiliary_retry_or_broker_authority_can_enter_the_one_shot_path() {
    for tail in [
        "--control", "--role control", "--view-only", "--attempts 2",
        "--clipboard", "--no-clipboard", "--audio", "--send /private/file",
        "--receive /private/directory", "--file-drop", "--capture-system-shortcuts",
        "--lease private-lease", "--connection-id private-instance", "--native-once",
        "--json --json", "--socket relative", "--port 0", "unexpected",
    ] {
        let failure = request(tail).err().expect("must refuse");
        assert_eq!(failure.exit, 2, "{tail}");
        assert!(!format!("{failure:?}").contains("private-"));
    }
}

#[test]
fn unsupported_artifacts_and_visibility_claims_refuse_before_any_io() {
    assert_eq!(code("--screenshot /private/screen.png"), "observation_artifact_unavailable");
    assert_eq!(code("--evidence-level instrumentally_observed"), "observation_evidence_unavailable");
    assert_eq!(code("--evidence-level decoded --evidence-level decoded"), "invalid_arguments");
    assert_eq!(code("--evidence-level invented"), "invalid_arguments");
}

#[test]
fn one_shot_keeps_explicit_transport_and_display_requirements() {
    for text in [
        "robot observe n-host --native-once --display only",
        "robot observe n-host --native-once --experimental-native",
        "robot observe n-host --native-once --experimental-native --display choose",
        "robot observe n-host --native-once --experimental-native --display 0",
        "robot observe n-host --json --native-once --experimental-native --display only",
    ] {
        assert!(parse(&args(text)).is_err(), "{text}");
    }
}

#[test]
fn legacy_commands_are_not_silently_converted_into_new_connections() {
    for text in [
        "robot observe n-host --json",
        "robot session open n-host --foreground --role view",
        "robot session status n-host --json",
        "connect n-host --view-only --experimental-native --display only",
    ] {
        assert!(!requested(&args(text)));
    }
    assert!(requested(&args("robot observe n-host --json --native-once")));
    assert!(parse(&args("robot observe n-host --json --native-once")).is_err());
}

#[test]
fn original_argument_size_and_count_are_checked_before_translation() {
    let mut oversized = args("robot observe n-host --native-once");
    oversized.extend((0..29).map(|_| "--json".to_owned()));
    assert!(parse(&oversized).is_err());
    assert_eq!(code(&format!("--worker /{}", "a".repeat(4096))), "invalid_arguments");
    let mut control = args("robot observe n-host --native-once");
    control[2] = "n-host\nPRIVATE".into();
    assert!(parse(&control).is_err());
}
