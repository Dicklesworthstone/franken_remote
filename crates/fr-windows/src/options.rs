//! Bounded command-line options shared by Windows and portable regression tests.
#![forbid(unsafe_code)]

use std::net::IpAddr;

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
pub(crate) struct Options {
    pub port: u16,
    /// Optional complete authenticated address-set constraint, not a DNS override.
    pub addresses: Vec<IpAddr>,
    pub family: Family,
    pub control: bool,
    /// With `--control`, share the text clipboard (default on; a host that
    /// does not share its clipboard drops it). `--no-clipboard` turns it off.
    pub clipboard: bool,
    pub seconds: Option<u64>,
    pub display: DisplayChoice,
}

impl Options {
    pub(crate) fn parse(mut rest: &[String], view: bool, default_port: u16) -> Option<Self> {
        let mut options = Self {
            port: default_port,
            addresses: Vec::new(),
            family: Family::Automatic,
            control: false,
            clipboard: true,
            seconds: None,
            display: DisplayChoice::Primary,
        };
        let mut port_set = false;
        let mut display_set = false;
        loop {
            match rest {
                [] => return Some(options),
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
                        _ => return None,
                    }
                    rest = tail;
                }
                [_] => return None,
            }
        }
    }
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
            })
        );
        assert_eq!(parse(&[], false), parse(&[], true));
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
            vec!["--no-clipboard", "--no-clipboard"],
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
        assert!(parse(&["--no-clipboard"], false).is_none());
        assert!(parse(&["--seconds", "1"], false).is_none());
    }

    #[test]
    fn clipboard_is_on_by_default_and_turned_off_only_by_its_flag() {
        assert!(parse(&["--control"], true).unwrap().clipboard);
        for args in [
            vec!["--control", "--no-clipboard"],
            vec!["--no-clipboard", "--control"],
        ] {
            let options = parse(&args, true).unwrap();
            assert!(options.control && !options.clipboard, "{args:?}");
        }
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
