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
fn host_choice_is_explicit_and_retains_all_connection_permissions_and_limits() {
    let opts = parsed("connect --choose-host --control --experimental-native --display choose --x-display :91 --socket /tmp/ts.sock --port 9443 --fit 960x540 --attempts 2 --no-clipboard --send /source --receive /destination --audio --json").unwrap();
    assert!(opts.json);
    assert_eq!(opts.socket, Some(PathBuf::from("/tmp/ts.sock")));
    let Command::Connect(connection) = opts.command else {
        panic!("connection")
    };
    assert!(connection.choose_host && connection.control);
    assert!(connection.target.node.is_empty());
    assert_eq!(connection.target.port, 9443);
    assert_eq!(connection.display, DisplayChoice::Choose);
    assert_eq!(connection.x_display.as_deref(), Some(":91"));
    assert_eq!(connection.fit_window, Some((960, 540)));
    assert_eq!(connection.attempts, 2);
    assert!(!connection.clipboard);
    assert_eq!(connection.upload.paths, [PathBuf::from("/source")]);
    assert!(connection.receive.is_some() && connection.audio.is_some());
}

#[test]
fn host_choice_never_changes_the_required_role_or_experimental_opt_in() {
    for args in [
        "connect --choose-host --experimental-native --display only",
        "connect --choose-host --view-only --control --experimental-native --display only",
        "connect --choose-host --view-only --display only",
        "connect --choose-host --view-only --experimental-native",
        "connect --choose-host --view-only --experimental-native --display only --send /file",
        "connect --choose-host --view-only --experimental-native --display only --receive /dest",
    ] {
        assert!(parsed(args).is_err(), "{args}");
    }
    let Command::Connect(connection) =
        parsed("connect n-explicit --view-only --experimental-native --display only")
            .unwrap()
            .command
    else {
        panic!("connection")
    };
    assert!(!connection.choose_host);
    assert_eq!(connection.target.node, "n-explicit");
}

#[test]
fn ambiguous_host_choices_and_nonconnection_commands_refuse_before_io() {
    for args in [
        "connect n-explicit --choose-host --view-only --experimental-native --display only",
        "connect --choose-host n-explicit --view-only --experimental-native --display only",
        "connect --choose-host --choose-host --view-only --experimental-native --display only",
        "connect --choose-host --saved --view-only --experimental-native --display only",
        "connect --choose-host --by-name --view-only --experimental-native --display only",
        "connect --choose-host --hosts-file /tmp/hosts.json --view-only --experimental-native --display only",
        "displays --choose-host --experimental-native",
        "hosts --choose-host",
        "doctor --choose-host",
        "inspect --choose-host",
    ] {
        assert!(parsed(args).is_err(), "{args}");
    }
}
