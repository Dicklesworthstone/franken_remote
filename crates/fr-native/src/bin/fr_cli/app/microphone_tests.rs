use super::*;

fn source() -> Microphone {
    Microphone {
        arguments: ["--microphone-source", "alsa_input.original"]
            .map(str::to_owned)
            .into(),
    }
}

fn viewer(control: bool) -> Vec<String> {
    [
        "connect",
        "nOriginal",
        "--experimental-native",
        "--display",
        "only",
        if control { "--control" } else { "--view-only" },
    ]
    .map(str::to_owned)
    .into()
}

#[test]
fn source_is_consumed_by_decline_and_view_only_without_device_arguments() {
    for control in [false, true] {
        let mut pending = Some(source());
        let arguments = viewer(control);
        assert_eq!(
            take_for_choice(&mut pending, arguments.clone(), false).unwrap(),
            arguments
        );
        assert!(pending.is_none());
    }
}

#[test]
fn consent_consumes_the_source_even_when_actual_role_or_build_refuses() {
    let mut pending = Some(source());
    assert!(take_for_choice(&mut pending, viewer(false), true).is_err());
    assert!(pending.is_none());
}

#[test]
fn availability_and_capture_consent_are_separate() {
    let selected = source();
    assert_eq!(available(Some(&selected)), build_available());
    assert_eq!(
        available(None),
        build_available() && cfg!(target_os = "macos")
    );
    assert!(!format!("{selected:?}").contains("alsa_input"));
    let mut pending = None;
    assert_eq!(
        take_for_choice(&mut pending, viewer(true), false).unwrap(),
        viewer(true)
    );
}

#[test]
fn source_syntax_is_validated_without_native_preparation() {
    assert!(Microphone::parse(Vec::new()).unwrap().is_none());
    for arguments in [
        vec!["--microphone-server", "/private/pulse"],
        vec!["--microphone-source", ""],
        vec!["--microphone-source", "invalid\nsource"],
    ] {
        assert!(Microphone::parse(arguments.into_iter().map(str::to_owned).collect()).is_err());
    }
}

#[test]
fn explicit_source_only_reaches_the_original_consented_connection() {
    let mut pending = Some(source());
    let result = take_for_choice(&mut pending, viewer(true), true);
    assert!(pending.is_none());
    if !build_available() {
        assert_eq!(result.unwrap_err().code, "app_microphone_unavailable");
        return;
    }
    let Command::Connect(connection) = options::parse(&result.unwrap()).unwrap().command else {
        panic!("expected viewer");
    };
    let selected = connection.microphone.unwrap();
    assert_eq!(selected.source, "alsa_input.original");
    assert!(selected.server.is_none());
    // A later picker can never recover the selected source from this owner.
    if !cfg!(target_os = "macos") {
        assert_eq!(
            take_for_choice(&mut pending, viewer(true), true)
                .unwrap_err()
                .code,
            "app_microphone_unavailable"
        );
    }
}

#[test]
fn explicit_server_is_retained_only_for_the_consented_viewer() {
    let arguments = [
        "--microphone-source",
        "alsa_input.original",
        "--microphone-server",
        "/private/pulse",
    ]
    .map(str::to_owned)
    .into();
    let parsed = Microphone::parse(arguments);
    if !build_available() {
        assert_eq!(parsed.unwrap_err().code, "app_microphone_unavailable");
        return;
    }
    let mut pending = parsed.unwrap();
    let Command::Connect(connection) =
        options::parse(&take_for_choice(&mut pending, viewer(true), true).unwrap())
            .unwrap()
            .command
    else {
        panic!("expected viewer");
    };
    assert_eq!(
        connection.microphone.unwrap().server.unwrap().to_str(),
        Some("/private/pulse")
    );
    assert!(pending.is_none());
}
