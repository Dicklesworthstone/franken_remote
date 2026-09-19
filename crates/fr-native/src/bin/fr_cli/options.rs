use super::Failure;
use std::{collections::BTreeSet, path::PathBuf};

pub struct Options {
    pub command: Command,
    pub json: bool,
    pub socket: Option<PathBuf>,
}
pub enum Command {
    Help,
    Hosts,
    Connect(Connection),
    Displays(Target),
    Doctor(DoctorOptions),
}
pub struct DoctorOptions {
    pub port: u16,
    pub roots: Option<PathBuf>,
}
pub struct Target {
    pub node: String,
    pub by_name: bool,
    pub roots: PathBuf,
    pub port: u16,
    pub ipv6: bool,
}
pub struct Connection {
    pub target: Target,
    pub display: DisplayChoice,
    pub worker: PathBuf,
    pub x_display: Option<String>,
    pub attempts: u8,
}
/// Explicit policies only. `Only` refuses ambiguity; it is never "first" or an
/// invented primary display. Both policies are reevaluated on each live catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayChoice {
    Handle(u128),
    Only,
    Choose,
}
impl DisplayChoice {
    pub fn select(self, catalog: &fr_wire::display::Catalog) -> Option<u128> {
        match self {
            Self::Handle(handle) => catalog.find(handle).map(|d| d.handle),
            // The session-owned native picker supplies this decision later.
            Self::Choose => None,
            Self::Only => match catalog.displays() {
                [display] => Some(display.handle),
                _ => None,
            },
        }
    }
}
fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Run fr --help; arguments are never echoed.",
        2,
    )
}
fn path(value: String) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(usage());
    }
    Ok(path)
}
/// Bounded argument grammar, no shell, URL, credential, arbitrary-command or
/// remote-policy option. `connect` without explicit view-only refuses; it never
/// silently substitutes viewing for the product's requested control behavior.
pub fn parse(args: &[String]) -> Result<Options, Failure> {
    if args.len() > 32
        || args
            .iter()
            .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
    {
        return Err(usage());
    }
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        return Ok(Options {
            command: Command::Help,
            json: false,
            socket: None,
        });
    }
    let (remote, doctor) = match args[0].as_str() {
        "hosts" => (false, false),
        "doctor" => (false, true),
        "connect" | "displays" => (true, false),
        _ => return Err(usage()),
    };
    let mut index = 1;
    let node = if remote {
        let node = args.get(index).ok_or_else(usage)?;
        if node.is_empty()
            || node.len() > 254
            || node.starts_with('-')
            || !node.is_ascii()
            || node
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b"/:?#@\\".contains(&b))
        {
            return Err(usage());
        }
        index += 1;
        Some(node.clone())
    } else {
        None
    };
    parse_command(args, index, node, args[0] == "displays", doctor)
}
fn parse_command(
    args: &[String],
    mut index: usize,
    node: Option<String>,
    inspect: bool,
    doctor: bool,
) -> Result<Options, Failure> {
    let remote = node.is_some();
    let connect = remote && !inspect;
    let mut seen = BTreeSet::new();
    let (mut json, mut socket, mut by_name, mut view_only, mut experimental, mut ipv6) =
        (false, None, false, false, false, false);
    let (mut display, mut worker, mut roots, mut x_display) = (None, None, None, None);
    let (mut port, mut attempts) = (8443_u16, 5_u8);
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        let value = |index: &mut usize| -> Result<String, Failure> {
            let value = args
                .get(*index)
                .filter(|s| !s.is_empty() && !s.starts_with("--"))
                .ok_or_else(usage)?
                .clone();
            *index += 1;
            Ok(value)
        };
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(path(value(&mut index)?)?),
            "--by-name" if remote => by_name = true,
            "--view-only" if connect => view_only = true,
            "--experimental-native" if remote => experimental = true,
            "--ipv6" if remote => ipv6 = true,
            "--display" if connect => {
                let choice = value(&mut index)?;
                display = Some(if choice == "only" {
                    DisplayChoice::Only
                } else if choice == "choose" {
                    DisplayChoice::Choose
                } else {
                    let handle = choice.parse::<u128>().map_err(|_| usage())?;
                    if handle == 0 {
                        return Err(usage());
                    }
                    DisplayChoice::Handle(handle)
                });
            }
            "--worker" if connect => worker = Some(path(value(&mut index)?)?),
            "--trust-roots" if remote || doctor => roots = Some(path(value(&mut index)?)?),
            "--x-display" if connect => x_display = Some(value(&mut index)?),
            "--port" if remote || doctor => {
                port = value(&mut index)?.parse().map_err(|_| usage())?
            }
            "--attempts" if connect => {
                attempts = value(&mut index)?.parse().map_err(|_| usage())?;
            }
            _ => return Err(usage()),
        }
    }
    let command = if let Some(node) = node {
        if connect && !view_only {
            return Err(Failure::new(
                "control_ui_unavailable",
                "This client currently requires --view-only; no control is silently granted or requested.",
                2,
            ));
        }
        if !experimental {
            return Err(Failure::new(
                "native_transport_unqualified",
                "Native transport is experimental; explicitly select --experimental-native only for development/qualification.",
                2,
            ));
        }
        if (!by_name && node.len() > 128) || port == 0 || !(1..=32).contains(&attempts) {
            return Err(usage());
        }
        let target = Target {
            node,
            by_name,
            roots: roots.ok_or_else(usage)?,
            port,
            ipv6,
        };
        if inspect {
            Command::Displays(target)
        } else {
            Command::Connect(Connection {
                target,
                display: display.ok_or_else(usage)?,
                worker: worker.ok_or_else(usage)?,
                x_display,
                attempts,
            })
        }
    } else if doctor {
        if port == 0 {
            return Err(usage());
        }
        Command::Doctor(DoctorOptions { port, roots })
    } else {
        Command::Hosts
    };
    Ok(Options {
        command,
        json,
        socket,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn options(s: &str) -> Result<Options, Failure> {
        parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }
    #[test]
    fn refuses_implicit_control_or_unqualified_transport_before_any_io() {
        assert_eq!(
            options("connect n-host").err().unwrap().code,
            "control_ui_unavailable"
        );
        assert_eq!(
            options("connect n-host --view-only").err().unwrap().code,
            "native_transport_unqualified"
        );
        assert!(options("connect n-host --view-only --experimental-native").is_err());
    }
    #[test]
    fn parses_only_explicit_bounded_connection_settings() {
        let o = options("connect n-host --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display 9 --ipv6 --attempts 2 --json").unwrap();
        let Command::Connect(c) = o.command else {
            panic!("connection required")
        };
        assert_eq!(c.target.node, "n-host");
        assert_eq!(c.display, DisplayChoice::Handle(9));
        assert_eq!(c.attempts, 2);
        assert!(c.target.ipv6 && o.json);
        assert!(!c.target.by_name);
    }
    #[test]
    fn unknown_duplicate_credential_and_policy_switches_are_not_ignored() {
        for s in [
            "hosts --json --json",
            "hosts --socket /a --socket /b",
            "hosts --port 8443",
            "hosts --socket ../sock",
            "connect https://host.invalid",
            "hosts --token secret",
            "connect n-peer --approval none",
            "hosts garbage",
        ] {
            assert!(options(s).is_err(), "{s}");
        }
    }
    #[test]
    fn rejects_oversized_and_control_character_arguments_without_echoing_them() {
        for a in [
            vec!["hosts".into(), "x".repeat(4097)],
            vec!["hosts".into(); 33],
            vec!["hosts".into(), "private\ncontent".into()],
        ] {
            let error = parse(&a).err().unwrap();
            assert!(!format!("{error:?}").contains("private"));
        }
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
    use fr_wire::display::{Catalog, Display};
    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }
    fn catalog(handles: &[u128]) -> Catalog {
        let displays = handles
            .iter()
            .map(|handle| Display {
                handle: *handle,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: -1920,
                y: 0,
                pixel_width: 1920,
                pixel_height: 1080,
                logical_width: 1280,
                logical_height: 720,
                scale_numerator: 3,
                scale_denominator: 2,
                rotation: 0,
            })
            .collect::<Vec<_>>();
        Catalog::new(1, &displays, &ProtocolLimits::ABSOLUTE).unwrap()
    }
    #[test]
    fn explicit_only_policy_refuses_zero_or_multiple_displays_and_rechecks_new_catalogs() {
        for handles in [&[][..], &[1, 2][..]] {
            assert_eq!(DisplayChoice::Only.select(&catalog(handles)), None);
        }
        assert_eq!(
            DisplayChoice::Only.select(&catalog(&[u128::MAX])),
            Some(u128::MAX)
        );
        assert_eq!(DisplayChoice::Handle(9).select(&catalog(&[9, 10])), Some(9));
        assert_eq!(DisplayChoice::Handle(9).select(&catalog(&[10])), None);
        assert_eq!(DisplayChoice::Only.select(&catalog(&[9, 10])), None);
    }
    #[test]
    fn inventory_needs_trust_and_experimental_opt_in_but_no_display_or_worker() {
        let o = parse(&args("displays n-peer --experimental-native --trust-roots /opt/fr/ca.pem --ipv6 --port 1234 --json")).unwrap();
        let Command::Displays(t) = o.command else {
            panic!("inventory required")
        };
        assert_eq!(t.node, "n-peer");
        assert_eq!(t.port, 1234);
        assert!(t.ipv6 && o.json);
        assert_eq!(
            parse(&args("displays n-peer --json")).err().unwrap().code,
            "native_transport_unqualified"
        );
        for extra in [
            "--worker /bin/false",
            "--display 9",
            "--view-only",
            "--attempts 2",
            "--x-display :0",
            "--port 0",
            "--token secret",
        ] {
            assert!(
                parse(&args(&format!(
                    "displays n-peer --experimental-native --trust-roots /opt/fr/ca.pem {extra}"
                )))
                .is_err()
            );
        }
    }
    #[test]
    fn connection_only_selection_is_explicit_and_handle_precision_is_preserved() {
        for choice in ["only".to_string(), u128::MAX.to_string()] {
            let o=parse(&args(&format!("connect n-peer --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display {choice}"))).unwrap();
            let Command::Connect(c) = o.command else {
                panic!("connection required")
            };
            assert_eq!(
                c.display,
                if choice == "only" {
                    DisplayChoice::Only
                } else {
                    DisplayChoice::Handle(u128::MAX)
                }
            );
        }
        for choice in [
            "0",
            "first",
            "primary",
            "-1",
            "340282366920938463463374607431768211456",
        ] {
            assert!(parse(&args(&format!("connect n-peer --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display {choice}"))).is_err());
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    #[test]
    fn native_choice_is_explicit_and_never_uses_the_first_or_only_alias() {
        let args = "connect n-peer --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display choose"
            .split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let Command::Connect(connection) = parse(&args).unwrap().command else {
            panic!("connection required")
        };
        assert_eq!(connection.display, DisplayChoice::Choose);
        let catalog =
            fr_wire::display::Catalog::new(1, &[], &fr_core::limits::ProtocolLimits::ABSOLUTE)
                .unwrap();
        assert_eq!(DisplayChoice::Choose.select(&catalog), None);
        let mut duplicate = args;
        duplicate.extend(["--display".into(), "only".into()]);
        assert!(parse(&duplicate).is_err());
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    #[test]
    fn parses_doctor_defaults_and_explicit_port() {
        let o = parse(&["doctor".into()]).unwrap();
        let doc = match o.command {
            Command::Doctor(doc) => doc,
            _ => return assert!(false, "doctor required"),
        };
        assert_eq!(doc.port, 8443);
        assert!(doc.roots.is_none());
        assert!(!o.json);
        assert!(o.socket.is_none());

        let o = parse(&[
            "doctor".into(),
            "--port".into(),
            "9443".into(),
            "--socket".into(),
            "/var/run/custom.sock".into(),
            "--trust-roots".into(),
            "/etc/ssl/roots.pem".into(),
            "--json".into(),
        ])
        .unwrap();
        let doc = match o.command {
            Command::Doctor(doc) => doc,
            _ => return assert!(false, "doctor required"),
        };
        assert_eq!(doc.port, 9443);
        assert_eq!(doc.roots, Some(PathBuf::from("/etc/ssl/roots.pem")));
        assert!(o.json);
        assert_eq!(o.socket, Some(PathBuf::from("/var/run/custom.sock")));
    }

    #[test]
    fn doctor_rejects_zero_port_or_unrelated_flags() {
        assert!(parse(&["doctor".into(), "--port".into(), "0".into()]).is_err());
        assert!(parse(&["doctor".into(), "--view-only".into()]).is_err());
        assert!(parse(&["doctor".into(), "--worker".into(), "/bin/false".into()]).is_err());
        assert!(parse(&["doctor".into(), "extra_positional".into()]).is_err());
        assert!(
            parse(&[
                "doctor".into(),
                "--port".into(),
                "8443".into(),
                "--port".into(),
                "8443".into()
            ])
            .is_err()
        );
    }
}
