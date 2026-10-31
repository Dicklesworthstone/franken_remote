//! Production CLI translation; no fabricated host or native readiness evidence.
use super::*;
use options::{Command, Connection, DisplayChoice, RobotCommand};
use std::path::Path;

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}
fn parsed(text: &str) -> options::Options {
    parse(&words(text)).unwrap()
}
fn connection(options: options::Options) -> Connection {
    let Command::Connect(connection) = options.command else {
        panic!("foreground command must use the canonical live connection");
    };
    connection
}
const NATIVE: &str = "--experimental-native --display only --worker /fixture/fr-media-worker";

#[test]
fn foreground_view_uses_the_real_connection_without_creating_control_or_auxiliary_lanes() {
    let options = parsed(&format!(
        "robot session open n-host --foreground --role view {NATIVE} --json"
    ));
    assert!(options.json);
    assert!(options.socket.is_none());
    let c = connection(options);
    assert_eq!(c.target.node, "n-host");
    assert!(!c.target.by_name && !c.target.ipv6);
    assert_eq!(c.target.port, 8443);
    assert_eq!(c.target.roots, Path::new(fr_tailnet::trust::SYSTEM_BUNDLE));
    assert_eq!(c.display, DisplayChoice::Only);
    assert_eq!(c.worker, Path::new("/fixture/fr-media-worker"));
    assert_eq!(c.attempts, 5);
    assert!(!c.control && !c.clipboard && !c.capture_system_shortcuts);
    assert!(c.audio.is_none());
    assert!(c.upload.paths.is_empty() && !c.upload.file_drop);
    assert!(c.receive.is_none());
}

#[test]
fn exact_target_trust_socket_display_and_reconnect_options_reach_the_canonical_owner() {
    let options = parsed(
        "robot session open host.fixture.ts.net --foreground --role view \
         --experimental-native --display 340282366920938463463374607431768211455 \
         --worker /fixture/worker --trust-roots /fixture/roots.pem \
         --socket /fixture/tailscaled.sock --by-name --ipv6 --port 9443 \
         --attempts 1 --x-display :7 --fit 960x540 --json",
    );
    assert!(options.json);
    assert_eq!(
        options.socket.as_deref(),
        Some(Path::new("/fixture/tailscaled.sock"))
    );
    let c = connection(options);
    assert_eq!(c.target.node, "host.fixture.ts.net");
    assert!(c.target.by_name && c.target.ipv6);
    assert_eq!(c.target.port, 9443);
    assert_eq!(c.target.roots, Path::new("/fixture/roots.pem"));
    assert_eq!(c.display, DisplayChoice::Handle(u128::MAX));
    assert_eq!(c.worker, Path::new("/fixture/worker"));
    assert_eq!(c.attempts, 1);
    assert_eq!(c.x_display.as_deref(), Some(":7"));
    assert_eq!(c.fit_window, Some((960, 540)));
}

#[test]
fn explicit_control_preserves_selected_capabilities_and_native_privacy_defaults() {
    let c = connection(parsed(&format!(
        "robot session open n-host --foreground --role control {NATIVE}"
    )));
    assert!(c.control && c.clipboard);
    assert!(!c.capture_system_shortcuts);
    let c = connection(parsed(&format!(
        "robot session open n-host --foreground --role control {NATIVE} \
         --no-clipboard --capture-system-shortcuts --send /fixture/selected \
         --receive /fixture/private --audio --audio-volume 35 --audio-muted"
    )));
    assert!(c.control && c.capture_system_shortcuts);
    assert!(!c.clipboard);
    assert_eq!(c.upload.paths.len(), 1);
    assert_eq!(c.upload.paths[0].as_path(), Path::new("/fixture/selected"));
    assert!(c.receive.is_some());
    let audio = c.audio.unwrap();
    assert_eq!(audio.volume, 35);
    assert!(audio.muted);
}

#[test]
fn foreground_open_cannot_bypass_the_existing_native_connection_refusals() {
    for (flags, code) in [
        ("", "native_transport_unqualified"),
        ("--experimental-native", "invalid_arguments"),
        (
            "--experimental-native --display only --control",
            "connection_role_required",
        ),
        (
            "--experimental-native --display only --clipboard",
            "clipboard_requires_control",
        ),
        (
            "--experimental-native --display only --send /fixture/selected",
            "send_requires_control",
        ),
        (
            "--experimental-native --display only --receive /fixture/private",
            "receive_requires_control",
        ),
        (
            "--experimental-native --display only --capture-system-shortcuts",
            "system_shortcuts_require_control",
        ),
        (
            "--experimental-native --display only --attempts 33",
            "invalid_arguments",
        ),
        (
            "--experimental-native --display only --trust-roots relative",
            "invalid_arguments",
        ),
    ] {
        let error = parse(&words(&format!(
            "robot session open n-private --foreground --role view {flags}"
        )))
        .err()
        .expect("must refuse before any I/O");
        assert_eq!(error.code, code, "{flags}");
        assert!(!format!("{error:?}").contains("n-private"));
    }
}

#[test]
fn duplicate_role_lifetime_and_lease_options_are_never_discarded() {
    for suffix in [
        "--foreground",
        "--role view",
        "--view-only",
        "--lease secret-lease",
        "--connection-id 0123456789abcdef0123456789abcdef",
        "--detach",
        "--json --json",
    ] {
        let error = parse(&words(&format!(
            "robot session open n-host --foreground --role view {NATIVE} {suffix}"
        )))
        .err()
        .expect("extra authority or duplicate option must refuse");
        assert!(!format!("{error:?}").contains("secret-lease"));
    }
    for prefix in [
        "--foreground",
        "--foreground --role",
        "--foreground --role admin",
        "--role view --foreground",
        "--json --foreground --role view",
    ] {
        assert!(
            parse(&words(&format!(
                "robot session open n-host {prefix} {NATIVE}"
            )))
            .is_err()
        );
    }
}

#[test]
fn foreground_marker_cannot_repair_a_missing_option_value() {
    for text in [
        "robot session open n-host --role view --socket --foreground /fixture/socket",
        "robot session open n-host --foreground --role view --experimental-native --display only --socket --foreground /fixture/socket",
    ] {
        assert!(parse(&words(text)).is_err());
    }
}

#[test]
fn original_argument_bounds_apply_before_translation() {
    let mut args = words(&format!(
        "robot session open n-host --foreground --role control {NATIVE}"
    ));
    while args.len() < 33 {
        args.push("--json".into());
    }
    assert_eq!(parse(&args).err().unwrap().code, "invalid_arguments");
    for target in ["x".repeat(4097), "n-host\nprivate".to_owned()] {
        let mut args = words(&format!(
            "robot session open n-host --foreground --role view {NATIVE}"
        ));
        args[3] = target;
        assert_eq!(parse(&args).err().unwrap().code, "invalid_arguments");
    }
}

#[test]
fn old_broker_commands_do_not_silently_turn_into_foreground_connections() {
    let options = parsed("robot session open n-host --role view --json");
    assert!(matches!(
        options.command,
        Command::Robot(RobotCommand::SessionOpen(_))
    ));
    assert!(options.json);
    assert!(matches!(
        parsed("robot session status n-host --json").command,
        Command::Robot(RobotCommand::SessionStatus { .. })
    ));
    assert!(matches!(
        parsed("robot session close n-host --json").command,
        Command::Robot(RobotCommand::SessionClose(_))
    ));
    assert!(matches!(parsed("--help").command, Command::Help));
}

#[test]
fn saved_aliases_are_not_implicitly_enabled_by_the_robot_translator() {
    for flag in ["--saved", "--hosts-file"] {
        let args = [
            "robot",
            "session",
            "open",
            "office",
            "--foreground",
            "--role",
            "view",
            "--experimental-native",
            "--display",
            "only",
            flag,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert!(super::parse(&args).is_err());
    }
}
