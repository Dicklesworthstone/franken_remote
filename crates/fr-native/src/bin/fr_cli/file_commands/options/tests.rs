use super::*;
const CONNECTION: &str = "0123456789abcdef0123456789abcdef";
const LEASE: &str = "fedcba9876543210fedcba9876543210";
fn parse(extra: &[&str]) -> Result<Options, Invalid> {
    let mut args = vec!["send", "n-host", "--connection-id", CONNECTION];
    args.extend_from_slice(extra);
    Options::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
}

#[test]
fn only_current_status_can_discover_a_file_lease() {
    let status = parse(&["--status", "--json"]).unwrap();
    assert_eq!(status.action, Action::Status(None));
    assert_eq!(status.connection, CONNECTION);
    assert!(status.lease.is_none() && status.json);
    for invalid in [
        vec!["--send", "/private/file"],
        vec!["--status", "--request", "1"],
        vec!["--cancel", "1"],
        vec!["--lease", LEASE],
    ] {
        assert_eq!(parse(&invalid), Err(Invalid));
    }
    assert_eq!(
        parse(&["--lease", LEASE, "--send", "/private/file"])
            .unwrap()
            .action,
        Action::Send(vec!["/private/file".into()])
    );
    assert_eq!(
        parse(&["--lease", LEASE, "--status", "--request", "17"])
            .unwrap()
            .action,
        Action::Status(Some(17))
    );
    assert_eq!(
        parse(&["--lease", LEASE, "--cancel", "17"]).unwrap().action,
        Action::Cancel(17)
    );
    let upper = LEASE.to_ascii_uppercase();
    assert_eq!(
        parse(&["--lease", &upper, "--status"])
            .unwrap()
            .lease
            .as_deref(),
        Some(LEASE)
    );
}

#[test]
fn explicit_paths_remain_absolute_bounded_and_unresolved() {
    for path in ["relative", "/a/../b", "/a/../../b", "/a\0b"] {
        assert_eq!(parse(&["--lease", LEASE, "--send", path]), Err(Invalid));
    }
    // Selection (including symlink refusal) belongs to the original worker;
    // parsing neither opens the missing path nor canonicalizes away links.
    let accepted = parse(&[
        "--lease",
        LEASE,
        "--send",
        "/no-such-local-upload/file",
        "--send",
        "/a/./b",
    ])
    .unwrap();
    assert_eq!(
        accepted.action,
        Action::Send(vec!["/no-such-local-upload/file".into(), "/a/./b".into()])
    );
    let mut args = vec!["--lease", LEASE];
    for _ in 0..8 {
        args.extend(["--send", "/explicit/file"]);
    }
    assert!(parse(&args).is_ok());
    args.extend(["--send", "/ninth"]);
    assert_eq!(parse(&args), Err(Invalid));
    let too_long = format!("/{}", "a".repeat(4096));
    assert_eq!(
        parse(&["--lease", LEASE, "--send", &too_long]),
        Err(Invalid)
    );
}

#[test]
fn no_ambiguous_action_or_handle_can_reach_ipc() {
    for invalid in [
        vec!["--status", "--status"],
        vec!["--status", "--json", "--json"],
        vec!["--status", "--connection-id", CONNECTION],
        vec!["--lease", LEASE, "--lease", LEASE, "--status"],
        vec!["--lease", LEASE, "--status", "--send", "/a"],
        vec!["--lease", LEASE, "--status", "--cancel", "1"],
        vec!["--lease", LEASE, "--cancel", "1", "--request", "1"],
        vec!["--lease", LEASE, "--request", "1"],
        vec!["--lease", LEASE, "--cancel", "0"],
        vec!["--lease", LEASE, "--cancel", "+1"],
        vec!["--lease", LEASE, "--cancel", "18446744073709551616"],
        vec!["--lease", "invalid", "--status"],
        vec!["--lease", LEASE, "--send"],
        vec!["--lease", LEASE, "/unflagged"],
    ] {
        assert_eq!(parse(&invalid), Err(Invalid), "{invalid:?}");
    }
    for args in [
        vec!["send", "n-host", "--status"],
        vec!["send", "n-host", "--connection-id", "bad", "--status"],
        vec![
            "send",
            "n-host\nother",
            "--connection-id",
            CONNECTION,
            "--status",
        ],
    ] {
        assert_eq!(
            Options::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
            Err(Invalid)
        );
    }
}
