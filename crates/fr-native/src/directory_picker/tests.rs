use super::*;

#[test]
fn local_directory_uris_are_decoded_once_without_remote_resolution() {
    for (uri, path) in [
        ("file:///home/user/Received", "/home/user/Received"),
        (
            "file://localhost/home/user/Received%20files",
            "/home/user/Received files",
        ),
        ("file:///home/user/100%25", "/home/user/100%"),
        ("file:///home/user/%E6%96%87%E4%BB%B6", "/home/user/文件"),
    ] {
        assert_eq!(selected_directory(uri), Ok(PathBuf::from(path)));
    }
}

#[test]
fn malformed_remote_and_multiple_directory_choices_refuse() {
    for uri in [
        "",
        "/home/user",
        "https://host/directory",
        "file://remote/home/user",
        "file:relative",
        "file:///home/user#fragment",
        "file:///home/user?query",
        "file:///home/%00bad",
        "file:///home/%2fother",
        "file:///home/%",
        "file:///home/%GG",
        "file:///home/%FF",
        "file:////home/user",
        "file:///home/a\nfile:///home/b",
        "#ignored\nfile:///home/a",
        "file:///home/a\r",
        "file:///",
    ] {
        assert_eq!(
            selected_directory(uri),
            Err(Error::InvalidSelection),
            "{uri:?}"
        );
    }
    assert_eq!(
        selected_directory(&format!("file:///{}", "a".repeat(4096))),
        Err(Error::InvalidSelection)
    );
}

#[test]
fn request_identity_retains_the_original_unique_sender_and_random_token() {
    assert_eq!(
        request_path(":1.23", c"fr_receive_0123")
            .unwrap()
            .to_bytes(),
        b"/org/freedesktop/portal/desktop/request/1_23/fr_receive_0123"
    );
    for sender in [
        "",
        "org.freedesktop.portal.Desktop",
        ":bad/path",
        ":1'",
        ":1",
    ] {
        assert_eq!(request_path(sender, c"good"), Err(Error::Unavailable));
    }
    assert_eq!(request_path(":1.23", c"bad/token"), Err(Error::Unavailable));
    let first = bus::token().unwrap();
    let second = bus::token().unwrap();
    assert_ne!(first, second);
    assert!(request_path(":1.23", &first).is_ok());
}

#[test]
fn production_only_connects_to_one_local_bus_transport() {
    assert!(local_address("unix:path=/run/user/1000/bus").is_ok());
    assert!(local_address("unix:abstract=/tmp/local-bus,guid=0123456789").is_ok());
    for address in [
        "tcp:host=127.0.0.1,port=1234",
        "unixexec:path=/tmp/run",
        "unix:path=/first;unix:path=/second",
        "unix:path=/tmp/\0bus",
        "",
    ] {
        assert_eq!(local_address(address), Err(Error::Unavailable));
    }
}

#[test]
fn errors_and_owner_debug_never_include_selected_paths() {
    let errors = [
        Error::Unavailable,
        Error::Cancelled,
        Error::Expired,
        Error::InvalidSelection,
        Error::CleanupIncomplete,
        Error::EventFlood,
        Error::Clock,
        Error::AlreadyUsed,
    ];
    for error in errors {
        assert!(!error.to_string().contains('/'));
    }
}

#[path = "tests/native.rs"]
mod native;
