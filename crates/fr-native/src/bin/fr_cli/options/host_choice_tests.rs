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
fn app_probe_requires_a_selection_only_installed_endpoint_picker() {
    let base = "connect --choose-host --view-only --experimental-native --display only";
    let Command::Connect(connection) = parsed(&format!("{base} --pick-only --app-probe"))
        .unwrap()
        .command
    else {
        panic!("picker")
    };
    assert!(connection.pick.unwrap().probe);
    assert!(!connection.control && connection.audio.is_none() && connection.microphone.is_none());
    for args in [
        format!("{base} --app-probe"),
        format!("{base} --pick-only --app-probe --socket /tmp/other.sock"),
        format!("{base} --pick-only --app-probe --app-probe"),
        "connect n-explicit --view-only --experimental-native --display only --app-probe".into(),
    ] {
        assert!(parsed(&args).is_err(), "{args}");
    }
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
    assert_eq!(connection.target.node, "");
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
fn expected_certificate_name_requires_one_exact_positional_identity() {
    let base = "connect n-selected --view-only --experimental-native --display only";
    for suffix in [
        "--expect-certificate-name",
        "--expect-certificate-name HOST.tail.ts.net",
        "--expect-certificate-name host.tail.ts.net.",
        "--expect-certificate-name host.tail.ts.net --by-name",
        "--saved --expect-certificate-name host.tail.ts.net",
        "--expect-certificate-name a.tail.ts.net --expect-certificate-name b.tail.ts.net",
    ] {
        assert!(parsed(&format!("{base} {suffix}")).is_err());
    }
    for command in [
        "connect --choose-host --view-only --experimental-native --display only --expect-certificate-name host.tail.ts.net",
        "displays n-selected --experimental-native --expect-certificate-name host.tail.ts.net",
    ] {
        assert!(parsed(command).is_err());
    }
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
