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
fn local_profile_commands_are_explicit_and_bounded() {
    let options = parsed("hosts save office n-peer --port 9443 --hosts-file /tmp/hosts.json --socket /tmp/api.sock --json").unwrap();
    assert!(options.json);
    assert_eq!(options.socket, Some(PathBuf::from("/tmp/api.sock")));
    let Command::SavedHosts(saved) = options.command else {
        panic!("saved host command");
    };
    assert_eq!(saved.file, Some(PathBuf::from("/tmp/hosts.json")));
    assert!(
        matches!(saved.action, SavedHostAction::Save { alias, node, port:9443 } if alias == "office" && node == "n-peer")
    );
    assert!(matches!(
        parsed("hosts saved --json").unwrap().command,
        Command::SavedHosts(SavedHostOptions {
            action: SavedHostAction::List,
            ..
        })
    ));
    assert!(
        matches!(parsed("hosts forget office").unwrap().command, Command::SavedHosts(SavedHostOptions { action: SavedHostAction::Forget { alias }, .. }) if alias == "office")
    );
    for command in [
        "hosts save Office n-peer",
        "hosts save office",
        "hosts save office n-peer --port 0",
        "hosts saved --socket /tmp/api",
        "hosts forget office --port 8443",
        "hosts saved --hosts-file relative",
        "hosts save office n-peer --by-name",
        "hosts saved --json --json",
    ] {
        assert!(parsed(command).is_err(), "{command}");
    }
}
#[test]
fn aliases_are_opt_in_and_never_override_profile_identity_or_port() {
    let Command::Displays(target) =
        parsed("displays office --saved --hosts-file /tmp/hosts.json --experimental-native")
            .unwrap()
            .command
    else {
        panic!("display command");
    };
    assert!(target.saved.is_some());
    let Command::Displays(target) = parsed("displays office --experimental-native")
        .unwrap()
        .command
    else {
        panic!("display command");
    };
    assert!(target.saved.is_none());
    for flags in [
        "--saved --by-name",
        "--saved --port 8443",
        "--hosts-file /tmp/hosts.json",
        "--saved --saved",
    ] {
        assert!(parsed(&format!("displays office --experimental-native {flags}")).is_err());
    }
    assert!(parsed("connect office --saved --experimental-native --display only").is_err());
    let Command::Connect(connection) =
        parsed("connect office --saved --experimental-native --display only --view-only")
            .unwrap()
            .command
    else {
        panic!("connect command");
    };
    assert!(!connection.control);
    assert!(!connection.clipboard);
    assert!(connection.target.saved.is_some());
}
