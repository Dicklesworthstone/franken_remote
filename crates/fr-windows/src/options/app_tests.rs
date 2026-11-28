use super::{AppCommand, ConnectionCommand};
use std::num::NonZeroU16;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn picker(port: u16) -> AppCommand {
    AppCommand::Picker {
        port: NonZeroU16::new(port).unwrap(),
        actual_pixels: false,
    }
}

#[test]
fn desktop_launch_defaults_and_explicit_ports_share_the_connection_grammar() {
    assert_eq!(AppCommand::parse(&[], 8443), Some(picker(8443)));
    assert_eq!(AppCommand::parse(&[], 47819), Some(picker(47819)));
    assert_eq!(AppCommand::parse(&[], 0), None);
    for (value, port) in [
        ("1", 1),
        ("8443", 8443),
        ("47819", 47819),
        ("65535", 65535),
        ("00080", 80),
    ] {
        assert_eq!(
            AppCommand::parse(&args(&["--port", value]), 8443),
            Some(picker(port))
        );
        for command in ["view", "directory"] {
            let arguments = args(&[command, "nHOST1", "--port", value]);
            assert_eq!(
                ConnectionCommand::parse(&arguments, 8443)
                    .unwrap()
                    .options
                    .port,
                port,
            );
        }
    }
}

#[test]
fn invalid_port_never_becomes_a_default_desktop_or_connection() {
    for value in [
        "", "0", "00000", "65536", "99999", "100000", "000080", "+80", "-80", " 80", "80 ", "80\n",
        "8\0", "8.0", "0x50", "８０", "--quiet",
    ] {
        assert_eq!(
            AppCommand::parse(&args(&["--port", value]), 8443),
            None,
            "{value:?}"
        );
        for command in ["view", "directory"] {
            let arguments = args(&[command, "nHOST1", "--port", value]);
            assert!(
                ConnectionCommand::parse(&arguments, 8443).is_none(),
                "{value:?}"
            );
        }
    }
    assert_eq!(AppCommand::parse(&args(&["--port"]), 8443), None);
}

#[test]
fn duplicate_and_malformed_app_flags_cannot_launch_or_install() {
    for arguments in [
        vec!["--port", "47819", "--port", "47819"],
        vec!["--port", "47819", "--port", "8443"],
        vec!["--port=47819"],
        vec!["--port", "47819", "unexpected"],
        vec!["--quiet", "--quiet"],
        vec!["--install", "--install"],
        vec!["--install", "--uninstall"],
        vec!["--install", "--port", "47819"],
        vec!["--port", "47819", "--install"],
        vec!["--uninstall", "--port", "47819"],
        vec!["--port", "47819", "--uninstall"],
        vec!["--session"],
        vec!["--session", "view", "nHOST1"],
    ] {
        assert_eq!(
            AppCommand::parse(&args(&arguments), 8443),
            None,
            "{arguments:?}"
        );
    }
}

#[test]
fn installation_and_quiet_behavior_remain_explicit() {
    for (arguments, expected) in [
        (vec!["--quiet"], picker(8443)),
        (vec!["--quiet", "--port", "47819"], picker(47819)),
        (vec!["--port", "47819", "--quiet"], picker(47819)),
        (vec!["--install"], AppCommand::Install { quiet: false }),
        (
            vec!["--install", "--quiet"],
            AppCommand::Install { quiet: true },
        ),
        (
            vec!["--quiet", "--install"],
            AppCommand::Install { quiet: true },
        ),
        (vec!["--uninstall"], AppCommand::Uninstall { quiet: false }),
        (
            vec!["--quiet", "--uninstall"],
            AppCommand::Uninstall { quiet: true },
        ),
    ] {
        assert_eq!(AppCommand::parse(&args(&arguments), 8443), Some(expected));
    }
}

#[test]
fn installed_app_relaunch_keeps_the_original_port_without_an_install_action() {
    for value in [1, 8443, 47819, 65535] {
        let port = NonZeroU16::new(value).unwrap();
        let relaunch = AppCommand::picker_args(port, false);
        assert_eq!(AppCommand::parse(&relaunch, 443), Some(picker(value)));
        assert_eq!(relaunch, ["--port".to_owned(), value.to_string()]);
    }
}

#[test]
fn native_pixels_follow_only_the_explicit_app_launch_and_installed_relaunch() {
    let port = NonZeroU16::new(47819).unwrap();
    let expected = AppCommand::Picker {
        port,
        actual_pixels: true,
    };
    let original = args(&["--actual-pixels", "--port", "47819"]);
    assert_eq!(AppCommand::parse(&original, 8443), Some(expected));
    let installed = AppCommand::picker_args(port, true);
    assert_eq!(AppCommand::parse(&installed, 443), Some(expected));
    assert_eq!(
        installed
            .iter()
            .filter(|arg| *arg == "--actual-pixels")
            .count(),
        1
    );
    assert_eq!(AppCommand::parse(&[], 8443), Some(picker(8443)));
    for arguments in [
        vec!["--actual-pixels", "--actual-pixels"],
        vec!["--install", "--actual-pixels"],
        vec!["--actual-pixels", "--uninstall"],
        vec!["--actual-pixels=false"],
    ] {
        assert_eq!(AppCommand::parse(&args(&arguments), 8443), None);
    }
}
