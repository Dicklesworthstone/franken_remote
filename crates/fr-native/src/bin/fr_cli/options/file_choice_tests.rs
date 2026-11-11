use super::*;

fn parsed(words: &str) -> Result<Options, Failure> {
    parse(
        &words
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}

#[test]
fn initial_file_choice_preserves_control_target_and_independent_permissions() {
    let options = parsed("connect --choose-host --control --experimental-native --display choose --choose-files --receive /private/inbox --file-drop --file-commands --no-clipboard --attempts 2 --json").unwrap();
    assert!(options.json);
    let Command::Connect(connection) = options.command else {
        panic!("connection");
    };
    assert!(connection.choose_host && connection.control && connection.upload.choose_files);
    assert!(connection.upload.paths.is_empty());
    assert!(connection.upload.file_drop && connection.upload.file_commands && connection.uploads());
    assert!(connection.receive.is_some());
    assert_eq!(connection.display, DisplayChoice::Choose);
    assert_eq!(connection.attempts, 2);
    assert!(!connection.clipboard && connection.audio.is_none() && connection.microphone.is_none());
}

#[test]
fn file_choice_refuses_viewing_ambiguous_sources_and_duplicate_flags() {
    for (arguments, code) in [
        (
            "connect n-host --view-only --experimental-native --display only --choose-files",
            "file_choice_requires_control",
        ),
        (
            "connect n-host --control --experimental-native --display only --choose-files --send /source",
            "file_choice_conflicts_with_send",
        ),
        (
            "connect n-host --control --experimental-native --display only --send /source --choose-files",
            "file_choice_conflicts_with_send",
        ),
        (
            "connect n-host --control --experimental-native --display only --choose-files --choose-files",
            "invalid_arguments",
        ),
        (
            "displays n-host --experimental-native --choose-files",
            "invalid_arguments",
        ),
        ("hosts --choose-files", "invalid_arguments"),
    ] {
        let failure = match parsed(arguments) {
            Ok(_) => panic!("unexpected choice: {arguments}"),
            Err(failure) => failure,
        };
        assert_eq!(failure.code, code, "{arguments}");
    }
}

#[test]
fn native_file_selection_is_never_enabled_by_plain_control_or_send() {
    for extra in ["", "--send /source", "--file-drop", "--file-commands"] {
        let options = parsed(&format!(
            "connect n-host --control --experimental-native --display only {extra}"
        ))
        .unwrap();
        let Command::Connect(connection) = options.command else {
            panic!("connection");
        };
        assert!(!connection.upload.choose_files);
    }
}
