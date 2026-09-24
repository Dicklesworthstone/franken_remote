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
    Status,
    Inspect(InspectOptions),
    Disconnect(DisconnectOptions),
    Robot(RobotCommand),
}
#[allow(dead_code)]
pub struct InspectOptions {
    pub node: String,
    pub by_name: bool,
    pub port: u16,
}
#[allow(dead_code)]
pub struct DisconnectOptions {
    pub node: String,
}
#[allow(dead_code)]
pub enum RobotCommand {
    SessionOpen(RobotSessionOpenOptions),
    SessionClose(RobotSessionCloseOptions),
    Observe(RobotObserveOptions),
    Input(RobotInputOptions),
}
#[allow(dead_code)]
pub struct RobotSessionOpenOptions {
    pub node: String,
    pub role: String,
    pub port: u16,
}
#[allow(dead_code)]
pub struct RobotSessionCloseOptions {
    pub node: String,
    pub lease: Option<String>,
}
#[allow(dead_code)]
pub struct RobotObserveOptions {
    pub node: String,
    pub display: Option<u32>,
    pub screenshot: Option<PathBuf>,
    pub evidence_level: Option<String>,
    pub port: u16,
}
#[allow(dead_code)]
pub struct RobotInputOptions {
    pub node: String,
    pub lease: String,
    pub request_id: String,
    pub batch: Option<PathBuf>,
    pub precondition_geometry: Option<u64>,
    pub max_observation_age_ms: Option<u64>,
    pub precondition_lease: Option<String>,
    pub precondition_focus: Option<String>,
    pub semantic_evidence: Option<String>,
    pub port: u16,
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
#[allow(dead_code)]
pub struct Connection {
    pub target: Target,
    pub display: DisplayChoice,
    pub worker: PathBuf,
    pub x_display: Option<String>,
    pub attempts: u8,
    pub fit_window: Option<(u32, u32)>,
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
    #[allow(dead_code)]
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

/// The media worker installed beside this `fr` executable.
fn sibling_worker() -> Result<PathBuf, Failure> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fr-media-worker")))
        .ok_or_else(usage)
}

fn validate_node(node: &str) -> Result<(), Failure> {
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
    Ok(())
}

fn next_arg<'a>(args: &'a [String], index: &mut usize) -> Result<&'a str, Failure> {
    let s = args
        .get(*index)
        .filter(|s| !s.is_empty() && !s.starts_with("--"))
        .ok_or_else(usage)?;
    *index += 1;
    Ok(s.as_str())
}

fn next_path(args: &[String], index: &mut usize) -> Result<PathBuf, Failure> {
    path(next_arg(args, index)?.to_string())
}

fn next_port(args: &[String], index: &mut usize) -> Result<u16, Failure> {
    let port: u16 = next_arg(args, index)?.parse().map_err(|_| usage())?;
    if port == 0 {
        return Err(usage());
    }
    Ok(port)
}

fn next_parsed<T: std::str::FromStr>(args: &[String], index: &mut usize) -> Result<T, Failure> {
    next_arg(args, index)?.parse().map_err(|_| usage())
}

fn parse_status(args: &[String]) -> Result<Options, Failure> {
    let mut index = 1;
    let mut json = false;
    let mut socket = None;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            _ => return Err(usage()),
        }
    }
    Ok(Options {
        command: Command::Status,
        json,
        socket,
    })
}

fn parse_inspect(args: &[String]) -> Result<Options, Failure> {
    let node = args.get(1).ok_or_else(usage)?;
    validate_node(node)?;
    let mut index = 2;
    let mut json = false;
    let mut socket = None;
    let mut port = 8443_u16;
    let mut by_name = false;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--by-name" => by_name = true,
            "--port" => port = next_port(args, &mut index)?,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            _ => return Err(usage()),
        }
    }
    Ok(Options {
        command: Command::Inspect(InspectOptions {
            node: node.clone(),
            by_name,
            port,
        }),
        json,
        socket,
    })
}

fn parse_disconnect(args: &[String]) -> Result<Options, Failure> {
    let node = args.get(1).ok_or_else(usage)?;
    validate_node(node)?;
    let mut index = 2;
    let mut json = false;
    let mut socket = None;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            _ => return Err(usage()),
        }
    }
    Ok(Options {
        command: Command::Disconnect(DisconnectOptions { node: node.clone() }),
        json,
        socket,
    })
}

#[allow(clippy::too_many_lines)]
fn parse_robot(args: &[String]) -> Result<Options, Failure> {
    let sub = args.get(1).ok_or_else(usage)?;
    match sub.as_str() {
        "session" => {
            let action = args.get(2).ok_or_else(usage)?;
            let node = args.get(3).ok_or_else(usage)?;
            validate_node(node)?;
            let mut index = 4;
            let mut json = false;
            let mut socket = None;
            let mut seen = BTreeSet::new();
            match action.as_str() {
                "open" => {
                    let mut role = "control".to_string();
                    let mut port = 8443_u16;
                    while index < args.len() {
                        let flag = args[index].as_str();
                        index += 1;
                        if !seen.insert(flag) {
                            return Err(usage());
                        }
                        match flag {
                            "--json" => json = true,
                            "--role" => {
                                let s = next_arg(args, &mut index)?;
                                if s != "view" && s != "control" {
                                    return Err(usage());
                                }
                                role = s.to_string();
                            }
                            "--port" => port = next_port(args, &mut index)?,
                            "--socket" => socket = Some(next_path(args, &mut index)?),
                            _ => return Err(usage()),
                        }
                    }
                    Ok(Options {
                        command: Command::Robot(RobotCommand::SessionOpen(
                            RobotSessionOpenOptions {
                                node: node.clone(),
                                role,
                                port,
                            },
                        )),
                        json,
                        socket,
                    })
                }
                "close" => {
                    let mut lease = None;
                    while index < args.len() {
                        let flag = args[index].as_str();
                        index += 1;
                        if !seen.insert(flag) {
                            return Err(usage());
                        }
                        match flag {
                            "--json" => json = true,
                            "--lease" => lease = Some(next_arg(args, &mut index)?.to_string()),
                            "--socket" => socket = Some(next_path(args, &mut index)?),
                            _ => return Err(usage()),
                        }
                    }
                    Ok(Options {
                        command: Command::Robot(RobotCommand::SessionClose(
                            RobotSessionCloseOptions {
                                node: node.clone(),
                                lease,
                            },
                        )),
                        json,
                        socket,
                    })
                }
                _ => Err(usage()),
            }
        }
        "observe" => {
            let node = args.get(2).ok_or_else(usage)?;
            validate_node(node)?;
            let mut index = 3;
            let mut json = false;
            let mut socket = None;
            let mut display = None;
            let mut screenshot = None;
            let mut evidence_level = None;
            let mut port = 8443_u16;
            let mut seen = BTreeSet::new();
            while index < args.len() {
                let flag = args[index].as_str();
                index += 1;
                if !seen.insert(flag) {
                    return Err(usage());
                }
                match flag {
                    "--json" => json = true,
                    "--display" => display = Some(next_parsed(args, &mut index)?),
                    "--screenshot" => screenshot = Some(next_path(args, &mut index)?),
                    "--evidence-level" => {
                        let s = next_arg(args, &mut index)?;
                        if s != "decoded"
                            && s != "submitted_to_compositor"
                            && s != "instrumentally_observed"
                        {
                            return Err(usage());
                        }
                        evidence_level = Some(s.to_string());
                    }
                    "--port" => port = next_port(args, &mut index)?,
                    "--socket" => socket = Some(next_path(args, &mut index)?),
                    _ => return Err(usage()),
                }
            }
            Ok(Options {
                command: Command::Robot(RobotCommand::Observe(RobotObserveOptions {
                    node: node.clone(),
                    display,
                    screenshot,
                    evidence_level,
                    port,
                })),
                json,
                socket,
            })
        }
        "input" => {
            let node = args.get(2).ok_or_else(usage)?;
            validate_node(node)?;
            let mut index = 3;
            let mut json = false;
            let mut socket = None;
            let mut lease = None;
            let mut request_id = None;
            let mut batch = None;
            let mut precondition_geometry = None;
            let mut max_observation_age_ms = None;
            let mut precondition_lease = None;
            let mut precondition_focus = None;
            let mut semantic_evidence = None;
            let mut port = 8443_u16;
            let mut seen = BTreeSet::new();
            while index < args.len() {
                let flag = args[index].as_str();
                index += 1;
                if !seen.insert(flag) {
                    return Err(usage());
                }
                match flag {
                    "--json" => json = true,
                    "--lease" => lease = Some(next_arg(args, &mut index)?.to_string()),
                    "--request-id" => request_id = Some(next_arg(args, &mut index)?.to_string()),
                    "--batch" => batch = Some(next_path(args, &mut index)?),
                    "--precondition-geometry" => {
                        precondition_geometry = Some(next_parsed(args, &mut index)?);
                    }
                    "--max-observation-age" => {
                        max_observation_age_ms = Some(next_parsed(args, &mut index)?);
                    }
                    "--precondition-lease" => {
                        precondition_lease = Some(next_arg(args, &mut index)?.to_string());
                    }
                    "--precondition-focus" => {
                        precondition_focus = Some(next_arg(args, &mut index)?.to_string());
                    }
                    "--semantic-evidence" => {
                        let s = next_arg(args, &mut index)?;
                        if s != "none"
                            && s != "unverified_pixels"
                            && s != "adapter"
                            && s != "instrumentation"
                        {
                            return Err(usage());
                        }
                        semantic_evidence = Some(s.to_string());
                    }
                    "--port" => port = next_port(args, &mut index)?,
                    "--socket" => socket = Some(next_path(args, &mut index)?),
                    _ => return Err(usage()),
                }
            }
            let lease = lease.ok_or_else(usage)?;
            let request_id = request_id.ok_or_else(usage)?;
            Ok(Options {
                command: Command::Robot(RobotCommand::Input(RobotInputOptions {
                    node: node.clone(),
                    lease,
                    request_id,
                    batch,
                    precondition_geometry,
                    max_observation_age_ms,
                    precondition_lease,
                    precondition_focus,
                    semantic_evidence,
                    port,
                })),
                json,
                socket,
            })
        }
        _ => Err(usage()),
    }
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
    match args[0].as_str() {
        "status" => parse_status(args),
        "inspect" => parse_inspect(args),
        "disconnect" => parse_disconnect(args),
        "robot" => parse_robot(args),
        "hosts" => parse_command(args, 1, None, false, false),
        "doctor" => parse_command(args, 1, None, false, true),
        "connect" | "displays" => {
            let node = args.get(1).ok_or_else(usage)?;
            validate_node(node)?;
            parse_command(args, 2, Some(node.clone()), args[0] == "displays", false)
        }
        _ => Err(usage()),
    }
}
fn fitted_size(dimensions: &str) -> Result<(u32, u32), Failure> {
    let (w, h) = dimensions.split_once('x').ok_or_else(usage)?;
    if w.is_empty()
        || h.is_empty()
        || !w.bytes().all(|c| c.is_ascii_digit())
        || !h.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(usage());
    }
    let width = w.parse::<u32>().map_err(|_| usage())?;
    let height = h.parse::<u32>().map_err(|_| usage())?;
    fr_media::worker::presentation::X11Target::new(1, width, height).map_err(|_| usage())?;
    Ok((width, height))
}
#[allow(clippy::too_many_lines)]
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
    let mut fit_window = None;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            "--by-name" if remote => by_name = true,
            "--view-only" if connect => view_only = true,
            "--experimental-native" if remote => experimental = true,
            "--ipv6" if remote => ipv6 = true,
            "--display" if connect => {
                let choice = next_arg(args, &mut index)?;
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
            "--fit" if connect => fit_window = Some(fitted_size(next_arg(args, &mut index)?)?),
            "--worker" if connect => worker = Some(next_path(args, &mut index)?),
            "--trust-roots" if remote || doctor => roots = Some(next_path(args, &mut index)?),
            "--x-display" if connect => x_display = Some(next_arg(args, &mut index)?.to_string()),
            "--port" if remote || doctor => port = next_port(args, &mut index)?,
            "--attempts" if connect => attempts = next_parsed(args, &mut index)?,
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
            roots: roots.unwrap_or_else(|| PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE)),
            port,
            ipv6,
        };
        if inspect {
            Command::Displays(target)
        } else {
            Command::Connect(Connection {
                target,
                display: display.ok_or_else(usage)?,
                worker: match worker {
                    Some(worker) => worker,
                    None => sibling_worker()?,
                },
                x_display,
                attempts,
                fit_window,
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
            unreachable!("connection required");
        };
        assert_eq!(c.target.node, "n-host");
        assert_eq!(c.display, DisplayChoice::Handle(9));
        assert_eq!(c.attempts, 2);
        assert!(c.target.ipv6 && o.json);
        assert!(!c.target.by_name);
    }
    #[test]
    fn trust_roots_and_worker_default_to_installed_locations() {
        let o = options("connect n-host --view-only --experimental-native --display only").unwrap();
        let Command::Connect(c) = o.command else {
            unreachable!("connection required");
        };
        assert_eq!(
            c.target.roots,
            PathBuf::from("/etc/ssl/certs/ca-certificates.crt")
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(c.worker, exe.parent().unwrap().join("fr-media-worker"));
        let o = options("displays n-host --experimental-native").unwrap();
        let Command::Displays(t) = o.command else {
            unreachable!("inspection required");
        };
        assert_eq!(t.roots, PathBuf::from("/etc/ssl/certs/ca-certificates.crt"));
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
            unreachable!("inventory required");
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
                unreachable!("connection required");
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
            unreachable!("connection required");
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
mod fit_tests {
    use super::*;
    const CONNECT: &str = "connect n-peer --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display only";
    fn options(s: &str) -> Result<Options, Failure> {
        parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }
    #[test]
    fn fit_is_an_explicit_bounded_local_rendering_choice() {
        for (suffix, expected) in [
            ("", None),
            (" --fit 960x540", Some((960, 540))),
            (" --fit 16x16", Some((16, 16))),
        ] {
            let Command::Connect(connection) =
                options(&format!("{CONNECT}{suffix}")).unwrap().command
            else {
                unreachable!("connection required");
            };
            assert_eq!(connection.fit_window, expected);
            assert_eq!(connection.display, DisplayChoice::Only);
        }
        assert_eq!(
            options("connect n-peer --fit 960x540").err().unwrap().code,
            "control_ui_unavailable"
        );
    }
    #[test]
    fn malformed_oversized_and_non_connection_fit_settings_refuse_before_io() {
        for value in [
            "",
            "0x0",
            "15x16",
            "960x539",
            "959x540",
            "4294967296x540",
            "16384x16384",
            "960X540",
            "960x540x2",
            "+960x540",
            "-960x540",
            "960x",
            "x540",
            "960.0x540",
            "960x540 --fit 320x240",
        ] {
            assert_eq!(
                options(&format!("{CONNECT} --fit {value}"))
                    .err()
                    .unwrap()
                    .code,
                "invalid_arguments"
            );
        }
        for command in [
            "hosts --fit 960x540",
            "displays n-peer --experimental-native --trust-roots /opt/fr/ca.pem --fit 960x540",
        ] {
            assert_eq!(options(command).err().unwrap().code, "invalid_arguments");
        }
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    #[test]
    fn parses_doctor_defaults_and_explicit_port() {
        let o = parse(&["doctor".into()]).unwrap();
        let Command::Doctor(doc) = o.command else {
            unreachable!("doctor required");
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
        let Command::Doctor(doc) = o.command else {
            unreachable!("doctor required");
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

#[cfg(test)]
mod robot_tests {
    use super::*;

    fn to_args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_status_inspect_disconnect_options() {
        let o = parse(&to_args("status --json")).unwrap();
        assert!(matches!(o.command, Command::Status) && o.json);

        let o = parse(&to_args("inspect host-alpha --port 9443 --by-name --json")).unwrap();
        let Command::Inspect(insp) = o.command else {
            panic!()
        };
        assert_eq!(
            (insp.node.as_str(), insp.port, insp.by_name, o.json),
            ("host-alpha", 9443, true, true)
        );

        let o = parse(&to_args("disconnect host-alpha --json")).unwrap();
        let Command::Disconnect(disc) = o.command else {
            panic!()
        };
        assert_eq!((disc.node.as_str(), o.json), ("host-alpha", true));
    }

    #[test]
    fn parses_robot_subcommands() {
        let o = parse(&to_args(
            "robot session open host-alpha --role control --json",
        ))
        .unwrap();
        assert!(matches!(
            o.command,
            Command::Robot(RobotCommand::SessionOpen(_))
        ));
        let Command::Robot(RobotCommand::SessionOpen(s)) = o.command else {
            return;
        };
        assert_eq!(
            (s.node.as_str(), s.role.as_str(), o.json),
            ("host-alpha", "control", true)
        );

        let o = parse(&to_args(
            "robot session close host-alpha --lease lease-123 --json",
        ))
        .unwrap();
        assert!(matches!(
            o.command,
            Command::Robot(RobotCommand::SessionClose(_))
        ));
        let Command::Robot(RobotCommand::SessionClose(s)) = o.command else {
            return;
        };
        assert_eq!(
            (s.node.as_str(), s.lease.as_deref()),
            ("host-alpha", Some("lease-123"))
        );

        let o = parse(&to_args("robot observe host-alpha --display 2 --screenshot /tmp/screen.png --evidence-level submitted_to_compositor --json")).unwrap();
        assert!(matches!(
            o.command,
            Command::Robot(RobotCommand::Observe(_))
        ));
        let Command::Robot(RobotCommand::Observe(obs)) = o.command else {
            return;
        };
        assert_eq!(
            (
                obs.node.as_str(),
                obs.display,
                obs.screenshot,
                obs.evidence_level.as_deref()
            ),
            (
                "host-alpha",
                Some(2),
                Some(PathBuf::from("/tmp/screen.png")),
                Some("submitted_to_compositor")
            )
        );

        let o = parse(&to_args("robot input host-alpha --lease lease-123 --request-id req-001 --precondition-lease lease-123 --precondition-focus Terminal --semantic-evidence adapter --json")).unwrap();
        assert!(matches!(o.command, Command::Robot(RobotCommand::Input(_))));
        let Command::Robot(RobotCommand::Input(inp)) = o.command else {
            return;
        };
        assert_eq!(
            (
                inp.node.as_str(),
                inp.lease.as_str(),
                inp.request_id.as_str(),
                inp.precondition_lease.as_deref(),
                inp.precondition_focus.as_deref(),
                inp.semantic_evidence.as_deref()
            ),
            (
                "host-alpha",
                "lease-123",
                "req-001",
                Some("lease-123"),
                Some("Terminal"),
                Some("adapter")
            )
        );
    }

    #[test]
    fn robot_rejects_missing_node_or_invalid_flags() {
        for s in [
            "robot session open",
            "robot session open host-alpha --role invalid_role",
            "robot input host-alpha --lease lease-123",
            "robot input host-alpha --request-id req-1",
            "robot input host-alpha --lease lease-1 --request-id req-1 --port 0",
            "inspect",
            "disconnect",
            "status extra_positional",
        ] {
            assert!(parse(&to_args(s)).is_err(), "{s}");
        }
    }
}
