//! Bounded command-line options shared by Windows and portable regression tests.
#![forbid(unsafe_code)]

use std::{net::IpAddr, path::PathBuf};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    #[default]
    Automatic,
    Ipv4,
    Ipv6,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayChoice {
    #[default]
    Primary,
    Only,
    Choose,
    Handle(u128),
}

impl DisplayChoice {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(Self::Primary),
            "only" => Some(Self::Only),
            "choose" => Some(Self::Choose),
            value if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => value
                .parse()
                .ok()
                .filter(|handle| *handle != 0)
                .map(Self::Handle),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReceiveOptions {
    pub directory: PathBuf,
    pub max_file_bytes: Option<u64>,
    pub max_session_bytes: Option<u64>,
    pub keep_both: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub port: u16,
    /// Optional complete authenticated address-set constraint, not a DNS override.
    pub addresses: Vec<IpAddr>,
    pub family: Family,
    pub control: bool,
    /// With `--control`, share text with a host that also enables clipboard.
    /// Defaults on; `--no-clipboard` disables it for this command.
    pub clipboard: bool,
    pub seconds: Option<u64>,
    pub display: DisplayChoice,
    /// One explicit regular file; consumed once by the original control grant.
    pub send: Option<PathBuf>,
    /// An explicit private destination; requires a controlling connection.
    pub receive: Option<ReceiveOptions>,
}

/// A missing viewing destination requests one local native machine choice.
/// Directory queries retain their explicit destination and never open a UI.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConnectionCommand<'a> {
    pub view: bool,
    pub selection: Option<&'a str>,
    pub options: Options,
}

impl<'a> ConnectionCommand<'a> {
    pub(crate) fn parse(args: &'a [String], default_port: u16) -> Option<Self> {
        let (view, rest) = match args {
            [] => (true, args),
            [command, rest @ ..] if command == "view" => (true, rest),
            [command, rest @ ..] if command == "directory" => (false, rest),
            _ => return None,
        };
        let (selection, rest) = match rest {
            [selection, rest @ ..] if !selection.is_empty() && !selection.starts_with('-') => {
                (Some(selection.as_str()), rest)
            }
            _ if view => (None, rest),
            _ => return None,
        };
        Some(Self {
            view,
            selection,
            options: Options::parse(rest, view, default_port)?,
        })
    }
}

impl Options {
    #[allow(clippy::too_many_lines)] // one bounded parser with no partial configuration
    pub(crate) fn parse(mut rest: &[String], view: bool, default_port: u16) -> Option<Self> {
        let mut options = Self {
            port: default_port,
            addresses: Vec::new(),
            family: Family::Automatic,
            control: false,
            clipboard: true,
            seconds: None,
            display: DisplayChoice::Primary,
            send: None,
            receive: None,
        };
        let mut port_set = false;
        let mut display_set = false;
        let mut receive_directory = None;
        let mut receive_file_bytes = None;
        let mut receive_session_bytes = None;
        let mut receive_conflict = None;
        loop {
            match rest {
                [] => {
                    if (options.send.is_some() || receive_directory.is_some()) && !options.control {
                        return None;
                    }
                    if matches!(
                        (receive_file_bytes, receive_session_bytes),
                        (Some(file), Some(session)) if session < file
                    ) {
                        return None;
                    }
                    options.receive = match receive_directory {
                        Some(directory) => Some(ReceiveOptions {
                            directory,
                            max_file_bytes: receive_file_bytes,
                            max_session_bytes: receive_session_bytes,
                            keep_both: receive_conflict.unwrap_or(false),
                        }),
                        None if receive_file_bytes.is_some()
                            || receive_session_bytes.is_some()
                            || receive_conflict.is_some() =>
                        {
                            return None;
                        }
                        None => None,
                    };
                    return Some(options);
                }
                [flag, tail @ ..] if view && flag == "--control" && !options.control => {
                    options.control = true;
                    rest = tail;
                }
                [flag, tail @ ..] if view && flag == "--no-clipboard" && options.clipboard => {
                    options.clipboard = false;
                    rest = tail;
                }
                [flag, tail @ ..]
                    if (flag == "--ipv4" || flag == "--ipv6")
                        && options.family == Family::Automatic =>
                {
                    options.family = if flag == "--ipv4" {
                        Family::Ipv4
                    } else {
                        Family::Ipv6
                    };
                    rest = tail;
                }
                [flag, value, tail @ ..] => {
                    match flag.as_str() {
                        "--port" if !port_set => {
                            options.port = value.parse().ok().filter(|port| *port != 0)?;
                            port_set = true;
                        }
                        "--address" if options.addresses.len() < 8 => {
                            let address: IpAddr = value.parse().ok()?;
                            if address.is_unspecified()
                                || address.is_loopback()
                                || address.is_multicast()
                                || matches!(address, IpAddr::V6(v) if v.to_ipv4_mapped().is_some())
                                || options.addresses.contains(&address)
                            {
                                return None;
                            }
                            options.addresses.push(address);
                        }
                        "--seconds" if view && options.seconds.is_none() => {
                            options.seconds = Some(value.parse().ok().filter(|s| *s != 0)?);
                        }
                        "--display" if view && !display_set => {
                            options.display = DisplayChoice::parse(value)?;
                            display_set = true;
                        }
                        "--send" if view && options.send.is_none() && !value.is_empty() => {
                            options.send = Some(PathBuf::from(value));
                        }
                        "--receive" if view && receive_directory.is_none() && !value.is_empty() => {
                            receive_directory = Some(PathBuf::from(value));
                        }
                        "--receive-max-file-bytes" if view && receive_file_bytes.is_none() => {
                            receive_file_bytes = Some(byte_limit(value)?);
                        }
                        "--receive-max-session-bytes"
                            if view && receive_session_bytes.is_none() =>
                        {
                            receive_session_bytes = Some(byte_limit(value)?);
                        }
                        "--receive-conflict" if view && receive_conflict.is_none() => {
                            receive_conflict = Some(match value.as_str() {
                                "reject" => false,
                                "keep-both" => true,
                                _ => return None,
                            });
                        }
                        _ => return None,
                    }
                    rest = tail;
                }
                [_] => return None,
            }
        }
    }
}

fn byte_limit(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|bytes| *bytes != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], view: bool) -> Option<Options> {
        Options::parse(
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            view,
            47819,
        )
    }

    #[test]
    fn explicit_downloads_require_control_and_keep_local_policy_independent() {
        let selected = parse(&["--control", "--receive", r"C:\Received"], true).unwrap();
        assert_eq!(
            selected.receive,
            Some(ReceiveOptions {
                directory: PathBuf::from(r"C:\Received"),
                max_file_bytes: None,
                max_session_bytes: None,
                keep_both: false,
            })
        );
        let duplex = parse(
            &[
                "--receive-max-file-bytes",
                "20",
                "--receive-conflict",
                "keep-both",
                "--send",
                r"C:\outbound.txt",
                "--control",
                "--receive",
                r"C:\Received",
                "--receive-max-session-bytes",
                "80",
            ],
            true,
        )
        .unwrap();
        let receive = duplex.receive.unwrap();
        assert_eq!(receive.max_file_bytes, Some(20));
        assert_eq!(receive.max_session_bytes, Some(80));
        assert!(receive.keep_both);
        assert!(duplex.send.is_some());
        assert!(duplex.clipboard);
    }

    #[test]
    fn invalid_download_policies_do_not_open_an_unapproved_destination() {
        for args in [
            vec!["--receive", r"C:\Received"],
            vec!["--control", "--receive", ""],
            vec!["--control", "--receive-max-file-bytes", "20"],
            vec!["--control", "--receive-max-session-bytes", "20"],
            vec!["--control", "--receive-conflict", "reject"],
        ] {
            assert!(parse(&args, true).is_none(), "{args:?}");
        }
        for tail in [
            vec!["--receive", r"C:\other"],
            vec!["--receive-conflict", "replace"],
            vec!["--receive-max-file-bytes", "0"],
            vec!["--receive-max-file-bytes", "+1"],
            vec!["--receive-max-session-bytes", "-1"],
            vec!["--receive-max-file-bytes", "18446744073709551616"],
            vec![
                "--receive-max-file-bytes",
                "20",
                "--receive-max-session-bytes",
                "10",
            ],
            vec![
                "--receive-conflict",
                "reject",
                "--receive-conflict",
                "keep-both",
            ],
            vec![
                "--receive-max-file-bytes",
                "1",
                "--receive-max-file-bytes",
                "2",
            ],
            vec![
                "--receive-max-session-bytes",
                "20",
                "--receive-max-session-bytes",
                "40",
            ],
        ] {
            let mut args = vec!["--control", "--receive", r"C:\Received"];
            args.extend(tail);
            assert!(parse(&args, true).is_none(), "{args:?}");
        }
        assert!(parse(&["--receive", r"C:\Received"], false).is_none());
        assert!(parse(&["--receive-conflict", "keep-both"], false).is_none());
    }

    #[test]
    fn a_native_machine_choice_preserves_explicit_connection_options() {
        for args in [vec![], vec!["view"]] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            let command = ConnectionCommand::parse(&args, 8443).unwrap();
            assert!(command.view);
            assert_eq!(command.selection, None);
            assert!(!command.options.control);
        }
        let args = [
            "view",
            "--control",
            "--no-clipboard",
            "--ipv6",
            "--display",
            "choose",
        ]
        .map(str::to_owned);
        let command = ConnectionCommand::parse(&args, 8443).unwrap();
        assert_eq!(command.selection, None);
        assert!(command.options.control);
        assert!(!command.options.clipboard);
        assert_eq!(command.options.family, Family::Ipv6);
        assert_eq!(command.options.display, DisplayChoice::Choose);
    }

    #[test]
    fn explicit_destinations_keep_the_existing_view_and_directory_paths() {
        for (verb, view) in [("view", true), ("directory", false)] {
            let args = [verb, "host.tail.ts.net", "--port", "9443", "--ipv4"].map(str::to_owned);
            let command = ConnectionCommand::parse(&args, 8443).unwrap();
            assert_eq!(command.view, view);
            assert_eq!(command.selection, Some("host.tail.ts.net"));
            assert_eq!(command.options.port, 9443);
            assert_eq!(command.options.family, Family::Ipv4);
        }
    }

    #[test]
    fn malformed_commands_do_not_silently_open_the_machine_picker() {
        for args in [
            vec!["directory"],
            vec!["directory", "--ipv6"],
            vec!["view", ""],
            vec!["view", "--unknown"],
            vec!["view", "--port"],
            vec!["view", "one.tail.ts.net", "two.tail.ts.net"],
            vec!["hosts", "--control"],
            vec!["--control"],
            vec!["unknown"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            assert!(ConnectionCommand::parse(&args, 8443).is_none(), "{args:?}");
        }
    }

    #[test]
    fn discovery_backed_connection_defaults_to_both_authenticated_families() {
        assert_eq!(
            parse(&[], true),
            Some(Options {
                port: 47819,
                addresses: Vec::new(),
                family: Family::Automatic,
                control: false,
                clipboard: true,
                seconds: None,
                display: DisplayChoice::Primary,
                send: None,
                receive: None,
            })
        );
        assert_eq!(parse(&[], false), parse(&[], true));
    }

    #[test]
    fn clipboard_defaults_on_for_control_and_explicit_off_is_order_independent() {
        assert!(parse(&["--control"], true).unwrap().clipboard);
        for args in [
            ["--control", "--no-clipboard"],
            ["--no-clipboard", "--control"],
        ] {
            let options = parse(&args, true).unwrap();
            assert!(options.control);
            assert!(!options.clipboard);
        }
        assert!(!parse(&["--no-clipboard"], true).unwrap().clipboard);
        assert!(parse(&["--no-clipboard", "--no-clipboard"], true).is_none());
        assert!(parse(&["--no-clipboard"], false).is_none());
    }

    #[test]
    fn a_single_explicit_file_requires_control_in_either_option_order() {
        for args in [
            vec!["--control", "--send", "C:\\Users\\me\\draft.txt"],
            vec!["--send", "C:\\Users\\me\\draft.txt", "--control"],
        ] {
            let options = parse(&args, true).unwrap();
            assert!(options.control);
            assert_eq!(
                options.send,
                Some(PathBuf::from("C:\\Users\\me\\draft.txt"))
            );
        }
        let unicode = "C:\\Users\\me\\Résumé final.txt";
        assert_eq!(
            parse(&["--control", "--send", unicode], true).unwrap().send,
            Some(PathBuf::from(unicode))
        );
        for args in [
            vec!["--send", "file.txt"],
            vec!["--control", "--send"],
            vec!["--control", "--send", ""],
            vec!["--control", "--send", "a", "--send", "b"],
        ] {
            assert!(parse(&args, true).is_none());
        }
        assert!(parse(&["--send", "file.txt"], false).is_none());
    }

    #[test]
    fn exact_addresses_and_explicit_family_choices_are_retained() {
        let options = parse(
            &[
                "--control",
                "--port",
                "54321",
                "--address",
                "fd7a:115c:a1e0::2",
                "--address",
                "100.64.0.2",
                "--ipv6",
                "--seconds",
                "30",
            ],
            true,
        )
        .unwrap();
        assert_eq!(options.port, 54321);
        assert_eq!(options.family, Family::Ipv6);
        assert!(options.control);
        assert_eq!(options.seconds, Some(30));
        assert_eq!(
            options.addresses,
            [
                "fd7a:115c:a1e0::2".parse::<IpAddr>().unwrap(),
                "100.64.0.2".parse::<IpAddr>().unwrap(),
            ]
        );
        assert_eq!(parse(&["--ipv4"], false).unwrap().family, Family::Ipv4);
    }

    #[test]
    fn conflicting_duplicate_and_malformed_options_refuse() {
        for args in [
            vec!["--ipv4", "--ipv6"],
            vec!["--ipv4", "--ipv4"],
            vec!["--control", "--control"],
            vec!["--port", "1", "--port", "2"],
            vec!["--seconds", "1", "--seconds", "2"],
            vec!["--address", "100.64.0.2", "--address", "100.64.0.2"],
            vec!["--port", "0"],
            vec!["--port", "65536"],
            vec!["--port", "one"],
            vec!["--seconds", "0"],
            vec!["--seconds", "-1"],
            vec!["--address", "host.fixture.ts.net"],
            vec!["--address", "0.0.0.0"],
            vec!["--address", "127.0.0.1"],
            vec!["--address", "224.0.0.1"],
            vec!["--address", "::"],
            vec!["--address", "::1"],
            vec!["--address", "ff02::1"],
            vec!["--address", "::ffff:100.64.0.2"],
            vec!["--unknown"],
            vec!["--port"],
            vec!["--address"],
            vec!["--seconds"],
        ] {
            assert!(parse(&args, true).is_none(), "{args:?}");
        }
        assert!(parse(&["--control"], false).is_none());
        assert!(parse(&["--seconds", "1"], false).is_none());
    }

    #[test]
    fn address_constraints_have_the_same_eight_address_bound_as_local_metadata() {
        let mut args = Vec::new();
        for last in 1..=8 {
            args.push("--address".to_owned());
            args.push(format!("100.64.0.{last}"));
        }
        assert_eq!(
            Options::parse(&args, false, 47819).unwrap().addresses.len(),
            8
        );
        args.extend(["--address".to_owned(), "100.64.0.9".to_owned()]);
        assert!(Options::parse(&args, false, 47819).is_none());
    }

    #[test]
    fn display_choice_is_explicit_and_does_not_change_control_or_address_policy() {
        for (value, expected) in [
            ("primary", DisplayChoice::Primary),
            ("only", DisplayChoice::Only),
            ("choose", DisplayChoice::Choose),
            ("7", DisplayChoice::Handle(7)),
            (
                "340282366920938463463374607431768211455",
                DisplayChoice::Handle(u128::MAX),
            ),
        ] {
            let options = parse(&["--display", value, "--control", "--ipv6"], true).unwrap();
            assert_eq!(options.display, expected);
            assert!(options.control);
            assert_eq!(options.family, Family::Ipv6);
            assert_eq!(options.addresses, [] as [IpAddr; 0]);
            assert!(parse(&["--display", value], false).is_none());
        }
        for args in [
            vec!["--display"],
            vec!["--display", "0"],
            vec!["--display", "-1"],
            vec!["--display", "+1"],
            vec!["--display", "1.0"],
            vec!["--display", "0x1"],
            vec!["--display", "first"],
            vec!["--display", "340282366920938463463374607431768211456"],
            vec!["--display", "primary", "--display", "only"],
            vec!["--display", "choose", "--display", "choose"],
        ] {
            assert!(parse(&args, true).is_none(), "{args:?}");
        }
    }
}
