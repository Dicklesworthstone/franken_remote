use super::*;
fn args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}
fn request(tail: &str) -> Result<Request, Failure> {
    parse_live(&args(&format!(
        "robot session open n-host --foreground --role view --publish-observations --experimental-native --display only {tail}"
    )))
}

#[test]
fn explicit_publisher_is_a_live_read_only_owner_with_original_configuration() {
    let value = request(
        "--socket /local/tailscaled.sock --trust-roots /local/roots.pem --x-display :9 --json",
    )
    .unwrap();
    assert!(value.live);
    assert!(value.json);
    assert!(!value.connection.control);
    assert!(!value.connection.clipboard);
    assert!(value.connection.audio.is_none());
    assert_eq!(value.socket, Some(PathBuf::from("/local/tailscaled.sock")));
    assert_eq!(value.connection.x_display.as_deref(), Some(":9"));
    assert_eq!(
        value.connection.target.roots,
        PathBuf::from("/local/roots.pem")
    );
}

#[test]
fn live_publication_cannot_enable_input_auxiliary_data_or_implicit_retries() {
    for tail in [
        "--control",
        "--audio",
        "--send /private/file",
        "--receive /private/dir",
        "--attempts 2",
        "--lease secret",
        "--publish-observations",
    ] {
        assert!(request(tail).is_err(), "{tail}");
    }
    let control = args(
        "robot session open n-host --foreground --role control --publish-observations --experimental-native --display only",
    );
    assert!(parse_live(&control).is_err());
}

#[test]
fn old_foreground_and_reserved_forms_do_not_silently_publish_or_change_lifetime() {
    for text in [
        "robot session open n-host --foreground --role view --experimental-native",
        "robot session open n-host --role view",
        "robot observe n-host --json",
    ] {
        assert!(!requested(&args(text)), "{text}");
    }
    let once = parse(&args(
        "robot observe n-host --native-once --experimental-native --display only",
    ))
    .unwrap();
    assert!(!once.live);
    assert!(requested(&args(
        "robot observe n-host --connection-id 00000000000000000000000000000001"
    )));
}

#[test]
fn publisher_validates_the_original_prefix_and_bounds_before_translation() {
    for text in [
        "robot session open n-host --role view --foreground --publish-observations",
        "robot session open n-host --foreground --role view --publish-observations",
        "robot session open n-host --foreground --role view --publish-observations --experimental-native",
    ] {
        assert!(parse_live(&args(text)).is_err());
    }
    let mut too_many = args(
        "robot session open n-host --foreground --role view --publish-observations --experimental-native --display only",
    );
    too_many.extend((0..32).map(|_| "--json".to_owned()));
    assert!(parse_live(&too_many).is_err());
}
