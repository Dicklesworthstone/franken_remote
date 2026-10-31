use super::Failure;
use std::{collections::BTreeSet, path::PathBuf};

#[path = "options/receive.rs"]
mod receive;
pub use receive::ReceiveOptions;
#[path = "options/local_session.rs"]
mod local_session;

pub struct Options {
    pub command: Command,
    pub json: bool,
    pub socket: Option<PathBuf>,
}
pub enum Command {
    Help,
    Hosts,
    SavedHosts(SavedHostOptions),
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
    pub probe: Option<InspectProbe>,
}
#[allow(dead_code)]
pub struct InspectProbe {
    pub roots: PathBuf,
    pub ipv6: bool,
}
#[allow(dead_code)]
pub struct DisconnectOptions {
    pub node: String,
}
#[allow(dead_code)]
pub enum RobotCommand {
    SessionOpen(RobotSessionOpenOptions),
    SessionClose(RobotSessionCloseOptions),
    SessionStatus { node: String },
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
    /// Comparison-only identifier from local session status, never a lease.
    pub connection_id: Option<String>,
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
pub struct SavedHostOptions {
    pub action: SavedHostAction,
    pub file: Option<PathBuf>,
}
pub enum SavedHostAction {
    Save {
        alias: String,
        node: String,
        port: u16,
    },
    List,
    Forget {
        alias: String,
    },
}
pub struct SavedSelection {
    pub file: Option<PathBuf>,
}
pub struct DoctorOptions {
    pub port: u16,
    pub roots: Option<PathBuf>,
}
pub struct Target {
    pub saved: Option<SavedSelection>,
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
    /// `--control`: request input control once, after fresh local evidence.
    /// False only with the explicit `--view-only`; never a silent default.
    pub control: bool,
    /// `--capture-system-shortcuts` (with `--control` only): request one
    /// keyboard grab from the original X11 input owner, after control is granted.
    pub capture_system_shortcuts: bool,
    /// Default-on with `--control`, disabled by `--no-clipboard`: let the
    /// UTF-8 text clipboard follow the control lease in both directions when
    /// both endpoints have a live local privacy control and native permission.
    pub clipboard: bool,
    /// `--audio`: explicitly ask for host playback audio in either role.
    /// `None` offers no audio capability at all.
    pub audio: Option<AudioRequest>,
    /// Explicit local upload choices, retained only for the original control.
    pub upload: UploadOptions,
    /// `--receive DIR`: the explicit local destination for host-selected files.
    /// This requires control and may accompany an explicit `--send` selection;
    /// neither direction is implicit.
    pub receive: Option<ReceiveOptions>,
}
#[cfg(any(feature = "linux-desktop", test))]
impl Connection {
    pub fn uploads(&self) -> bool {
        self.upload.file_drop || !self.upload.paths.is_empty()
    }
}
#[allow(dead_code)]
pub struct UploadOptions {
    /// `--send PATH` (repeatable, with `--control` only): files or directories
    /// explicitly selected for the original upload. Empty selects no source.
    pub paths: Vec<PathBuf>,
    /// `--file-drop`: explicitly allow a first local Copy drop after control
    /// without requiring an initial --send. Reconnect never re-enables it.
    pub file_drop: bool,
}
/// At most this many `--send` files per connection (frd's native bound).
pub const MAX_SENDS: usize = 8;
/// Local playback choices; never a peer value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRequest {
    /// Absolute local `PulseAudio` socket; `None` resolves `PULSE_SERVER`,
    /// then `$XDG_RUNTIME_DIR/pulse/native`, at connect time.
    pub server: Option<PathBuf>,
    /// Output sink; `None` is the local server's default output.
    pub sink: Option<String>,
    /// Local remote-audio gain in percent; never the host or OS mixer volume.
    pub volume: u8,
    /// Initial local mute, independent of gain.
    pub muted: bool,
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
    let (mut probe, mut experimental, mut ipv6) = (false, false, false);
    let mut roots = None;
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
            "--probe" => probe = true,
            "--experimental-native" => experimental = true,
            "--trust-roots" => roots = Some(next_path(args, &mut index)?),
            "--ipv6" => ipv6 = true,
            _ => return Err(usage()),
        }
    }
    if !probe && (experimental || roots.is_some() || ipv6) {
        return Err(usage());
    }
    if probe && !experimental {
        return Err(Failure::new(
            "native_transport_unqualified",
            "A capability probe opens the experimental native transport; explicitly select --experimental-native.",
            2,
        ));
    }
    Ok(Options {
        command: Command::Inspect(InspectOptions {
            node: node.clone(),
            by_name,
            port,
            probe: probe.then(|| InspectProbe {
                roots: roots.unwrap_or_else(|| PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE)),
                ipv6,
            }),
        }),
        json,
        socket,
    })
}

fn parse_saved_hosts(args: &[String]) -> Result<Options, Failure> {
    use fr_client::discovery::saved_peers::valid_alias;
    let action = args.get(1).ok_or_else(usage)?.as_str();
    let alias = if action == "saved" {
        None
    } else {
        Some(
            args.get(2)
                .filter(|s| valid_alias(s))
                .ok_or_else(usage)?
                .clone(),
        )
    };
    let node = if action == "save" {
        let node = args.get(3).ok_or_else(usage)?;
        validate_node(node)?;
        if node.len() > 128 {
            return Err(usage());
        }
        Some(node.clone())
    } else {
        None
    };
    let mut index = match action {
        "save" => 4,
        "forget" => 3,
        "saved" => 2,
        _ => return Err(usage()),
    };
    let (mut json, mut file, mut socket, mut port) = (false, None, None, 8443);
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--hosts-file" => file = Some(next_path(args, &mut index)?),
            "--socket" if action == "save" => socket = Some(next_path(args, &mut index)?),
            "--port" if action == "save" => port = next_port(args, &mut index)?,
            _ => return Err(usage()),
        }
    }
    let action = match action {
        "save" => SavedHostAction::Save {
            alias: alias.ok_or_else(usage)?,
            node: node.ok_or_else(usage)?,
            port,
        },
        "forget" => SavedHostAction::Forget {
            alias: alias.ok_or_else(usage)?,
        },
        _ => SavedHostAction::List,
    };
    Ok(Options {
        command: Command::SavedHosts(SavedHostOptions { action, file }),
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
                "status" => local_session::status(args),
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
                    let mut connection_id = None;
                    while index < args.len() {
                        let flag = args[index].as_str();
                        index += 1;
                        if !seen.insert(flag) {
                            return Err(usage());
                        }
                        match flag {
                            "--json" => json = true,
                            "--lease" => lease = Some(next_arg(args, &mut index)?.to_string()),
                            "--connection-id" => {
                                connection_id =
                                    Some(local_session::identifier(next_arg(args, &mut index)?)?);
                            }
                            "--socket" => socket = Some(next_path(args, &mut index)?),
                            _ => return Err(usage()),
                        }
                    }
                    Ok(Options {
                        command: Command::Robot(RobotCommand::SessionClose(
                            RobotSessionCloseOptions {
                                node: node.clone(),
                                lease,
                                connection_id,
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
/// remote-policy option. `connect` requires exactly one explicit role,
/// `--view-only` or `--control`; control is requested once per invocation.
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
        "hosts"
            if args
                .get(1)
                .is_some_and(|s| matches!(s.as_str(), "save" | "saved" | "forget")) =>
        {
            parse_saved_hosts(args)
        }
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
    let (mut control, mut clipboard, mut capture_system_shortcuts) = (false, None, false);
    let (mut audio, mut audio_server, mut audio_sink) = (false, None, None);
    let (mut audio_volume, mut audio_muted) = (None, false);
    let (mut display, mut worker, mut roots, mut x_display) = (None, None, None, None);
    let (mut port, mut attempts) = (8443_u16, 5_u8);
    let mut fit_window = None;
    let (mut saved, mut hosts_file) = (false, None);
    let mut send = Vec::new();
    let mut file_drop = false;
    let mut receive = None;
    let (mut receive_max_file_bytes, mut receive_max_session_bytes, mut receive_keep_both) =
        (None, None, None);
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        // `--send` is the one repeatable flag; its count is bounded below.
        if flag != "--send" && !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            "--by-name" if remote => by_name = true,
            "--saved" if remote => saved = true,
            "--hosts-file" if remote => hosts_file = Some(next_path(args, &mut index)?),
            "--view-only" if connect => view_only = true,
            "--control" if connect => control = true,
            "--capture-system-shortcuts" if connect => capture_system_shortcuts = true,
            "--clipboard" | "--no-clipboard" if connect && clipboard.is_none() => {
                clipboard = Some(flag == "--clipboard");
            }
            "--audio" if connect => audio = true,
            "--audio-server" if connect => audio_server = Some(next_path(args, &mut index)?),
            "--audio-sink" if connect => {
                audio_sink = Some(next_arg(args, &mut index)?.to_string());
            }
            "--audio-volume" if connect => {
                let volume: u8 = next_parsed(args, &mut index)?;
                if volume > 100 {
                    return Err(usage());
                }
                audio_volume = Some(volume);
            }
            "--audio-muted" if connect => audio_muted = true,
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
            "--send" if connect => send.push(next_path(args, &mut index)?),
            "--file-drop" if connect => file_drop = true,
            "--receive" if connect => receive = Some(next_path(args, &mut index)?),
            "--receive-max-file-bytes" if connect => {
                receive_max_file_bytes = Some(receive::bytes(next_arg(args, &mut index)?)?);
            }
            "--receive-max-session-bytes" if connect => {
                receive_max_session_bytes = Some(receive::bytes(next_arg(args, &mut index)?)?);
            }
            "--receive-conflict" if connect => {
                receive_keep_both = Some(receive::keep_both(next_arg(args, &mut index)?)?);
            }
            _ => return Err(usage()),
        }
    }
    let receive = ReceiveOptions::new(
        receive,
        receive_max_file_bytes,
        receive_max_session_bytes,
        receive_keep_both,
    )?;
    if hosts_file.is_some() && !saved {
        return Err(usage());
    }
    let command = if let Some(node) = node {
        if saved
            && (by_name
                || seen.contains("--port")
                || !fr_client::discovery::saved_peers::valid_alias(&node))
        {
            return Err(usage());
        }
        if connect && view_only == control {
            return Err(Failure::new(
                "connection_role_required",
                "Choose exactly one of --view-only or --control. Control is requested once; transient reconnects after that request restore viewing only.",
                2,
            ));
        }
        if clipboard == Some(true) && !control {
            return Err(Failure::new(
                "clipboard_requires_control",
                "The clipboard follows a control lease; pass --control with --clipboard. A view-only session never exposes either clipboard.",
                2,
            ));
        }
        if capture_system_shortcuts && !control {
            return Err(Failure::new(
                "system_shortcuts_require_control",
                "System-shortcut capture requires --control; a view-only connection never grabs the local keyboard.",
                2,
            ));
        }
        if !audio
            && (audio_server.is_some()
                || audio_sink.is_some()
                || audio_volume.is_some()
                || audio_muted)
        {
            return Err(usage());
        }
        if !send.is_empty() && !control {
            return Err(Failure::new(
                "send_requires_control",
                "Files are sent only on your own control lease; pass --control with --send. A view-only session never sends files.",
                2,
            ));
        }
        if file_drop && !control {
            return Err(Failure::new(
                "file_drop_requires_control",
                "File drops use your original control lease; pass --control with --file-drop. A view-only session never selects or sends local files.",
                2,
            ));
        }
        if receive.is_some() && !control {
            return Err(Failure::new(
                "receive_requires_control",
                "Files are received only on your own control lease; pass --control with --receive. A view-only session never receives files.",
                2,
            ));
        }
        if send.len() > MAX_SENDS {
            return Err(Failure::new(
                "send_too_many",
                "At most 8 initial --send selections; after they finish, copy another selection onto the viewer while its upload panel is open.",
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
            saved: saved.then_some(SavedSelection { file: hosts_file }),
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
                control,
                capture_system_shortcuts,
                clipboard: control && clipboard.unwrap_or(true),
                audio: audio.then_some(AudioRequest {
                    server: audio_server,
                    sink: audio_sink,
                    volume: audio_volume.unwrap_or(100),
                    muted: audio_muted,
                }),
                upload: UploadOptions {
                    paths: send,
                    file_drop,
                },
                receive,
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
        for implicit in ["connect n-host", "connect n-host --view-only --control"] {
            assert_eq!(
                options(implicit).err().unwrap().code,
                "connection_role_required",
                "{implicit}"
            );
        }
        for role in ["--view-only", "--control"] {
            assert_eq!(
                options(&format!("connect n-host {role}"))
                    .err()
                    .unwrap()
                    .code,
                "native_transport_unqualified"
            );
            // A role and transport opt-in still need an explicit display policy.
            assert!(options(&format!("connect n-host {role} --experimental-native")).is_err());
        }
    }
    #[test]
    fn control_is_an_explicit_connection_role_exclusive_with_view_only() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        for (role, control) in [("--control", true), ("--view-only", false)] {
            let Command::Connect(c) = options(&format!("connect {base} {role}")).unwrap().command
            else {
                unreachable!("connection required");
            };
            assert_eq!(c.control, control, "{role}");
        }
        for refused in [
            format!("connect {base} --control --control"),
            "displays n-host --experimental-native --control".into(),
            "doctor --control".into(),
            "hosts --control".into(),
        ] {
            assert_eq!(
                options(&refused).err().unwrap().code,
                "invalid_arguments",
                "{refused}"
            );
        }
    }
    #[test]
    fn clipboard_defaults_to_control_and_can_be_disabled_before_connecting() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        for (flags, control, clipboard) in [
            ("--control", true, true),
            ("--control --clipboard", true, true),
            ("--control --no-clipboard", true, false),
            ("--view-only", false, false),
            ("--view-only --no-clipboard", false, false),
        ] {
            let Command::Connect(c) = options(&format!("connect {base} {flags}")).unwrap().command
            else {
                unreachable!("connection required");
            };
            assert_eq!(c.control, control, "{flags}");
            assert_eq!(c.clipboard, clipboard, "{flags}");
        }
        // Never with viewing only: a typed refusal before any I/O.
        assert_eq!(
            options(&format!("connect {base} --view-only --clipboard"))
                .err()
                .unwrap()
                .code,
            "clipboard_requires_control"
        );
        for refused in [
            format!("connect {base} --control --clipboard --clipboard"),
            format!("connect {base} --control --no-clipboard --no-clipboard"),
            format!("connect {base} --control --clipboard --no-clipboard"),
            format!("connect {base} --control --no-clipboard --clipboard"),
            "displays n-host --experimental-native --clipboard".into(),
            "displays n-host --experimental-native --no-clipboard".into(),
            "doctor --clipboard".into(),
            "doctor --no-clipboard".into(),
            "hosts --clipboard".into(),
            "hosts --no-clipboard".into(),
        ] {
            assert_eq!(
                options(&refused).err().unwrap().code,
                "invalid_arguments",
                "{refused}"
            );
        }
    }
    #[test]
    fn system_shortcut_capture_requires_an_explicit_controlled_connection() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        for (flags, capture) in [
            ("--view-only", false),
            ("--control", false),
            ("--control --capture-system-shortcuts", true),
        ] {
            let Command::Connect(connection) =
                options(&format!("connect {base} {flags}")).unwrap().command
            else {
                unreachable!("connection required");
            };
            assert_eq!(connection.capture_system_shortcuts, capture, "{flags}");
        }
        assert_eq!(
            options(&format!(
                "connect {base} --view-only --capture-system-shortcuts"
            ))
            .err()
            .unwrap()
            .code,
            "system_shortcuts_require_control"
        );
        assert_eq!(
            options("connect n-host --control --capture-system-shortcuts")
                .err()
                .unwrap()
                .code,
            "native_transport_unqualified"
        );
        assert_eq!(
            options(&format!("connect {base} --capture-system-shortcuts"))
                .err()
                .unwrap()
                .code,
            "connection_role_required"
        );
    }
    #[test]
    fn duplicate_or_non_connection_system_shortcut_capture_is_rejected_before_io() {
        for refused in [
            "connect n-host --control --experimental-native --display only --capture-system-shortcuts --capture-system-shortcuts",
            "displays n-host --experimental-native --capture-system-shortcuts",
            "doctor --capture-system-shortcuts",
            "hosts --capture-system-shortcuts",
        ] {
            assert_eq!(
                options(refused).err().unwrap().code,
                "invalid_arguments",
                "{refused}"
            );
        }
    }
    #[test]
    fn audio_is_an_explicit_request_with_local_selection_in_either_role() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        let Command::Connect(plain) = options(&format!("connect {base} --view-only"))
            .unwrap()
            .command
        else {
            unreachable!("connection required");
        };
        assert_eq!(plain.audio, None);
        let Command::Connect(c) = options(&format!(
            "connect {base} --view-only --audio --audio-server /run/user/7/pulse/native --audio-sink speakers"
        ))
        .unwrap()
        .command
        else {
            unreachable!("connection required");
        };
        assert_eq!(
            c.audio,
            Some(AudioRequest {
                server: Some(PathBuf::from("/run/user/7/pulse/native")),
                sink: Some("speakers".into()),
                volume: 100,
                muted: false,
            })
        );
        // Control uses the same isolated output; neither role enables audio
        // implicitly or lets an audio flag substitute for an explicit role.
        let Command::Connect(controlled) = options(&format!(
            "connect {base} --control --audio --audio-server /run/native --audio-sink speakers"
        ))
        .unwrap()
        .command
        else {
            unreachable!("connection required");
        };
        assert!(controlled.control);
        assert_eq!(
            controlled.audio,
            Some(AudioRequest {
                server: Some(PathBuf::from("/run/native")),
                sink: Some("speakers".into()),
                volume: 100,
                muted: false,
            })
        );
        let Command::Connect(plain_control) = options(&format!("connect {base} --control"))
            .unwrap()
            .command
        else {
            unreachable!("connection required");
        };
        assert!(plain_control.audio.is_none());
        for refused in [
            format!("connect {base} --view-only --audio-sink speakers"),
            format!("connect {base} --view-only --audio-server /run/native"),
            format!("connect {base} --view-only --audio --audio"),
            "displays n-host --experimental-native --audio".into(),
            "hosts --audio".into(),
        ] {
            assert_eq!(
                options(&refused).err().unwrap().code,
                "invalid_arguments",
                "{refused}"
            );
        }
    }
    #[test]
    fn local_audio_preferences_require_audio_and_work_in_both_roles() {
        let base = "connect n-host --experimental-native --display only";
        for role in ["--view-only", "--control"] {
            for volume in [0, 37, 100] {
                let Command::Connect(connection) = options(&format!(
                    "{base} {role} --audio --audio-volume {volume} --audio-muted"
                ))
                .unwrap()
                .command
                else {
                    unreachable!("connection required");
                };
                let audio = connection.audio.unwrap();
                assert_eq!(audio.volume, volume);
                assert!(audio.muted);
            }
            for flags in [
                "--audio-volume 30",
                "--audio-muted",
                "--audio --audio-volume 101",
                "--audio --audio-volume -1",
                "--audio --audio-volume NaN",
                "--audio --audio-volume 0.5",
                "--audio --audio-volume 10 --audio-volume 20",
                "--audio --audio-muted --audio-muted",
            ] {
                assert_eq!(
                    options(&format!("{base} {role} {flags}"))
                        .err()
                        .unwrap()
                        .code,
                    "invalid_arguments",
                    "{flags}"
                );
            }
        }
    }
    #[test]
    fn send_is_a_repeatable_bounded_control_only_selection() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        let Command::Connect(plain) = options(&format!("connect {base} --control"))
            .unwrap()
            .command
        else {
            unreachable!("connection required");
        };
        assert_eq!(plain.upload.paths, Vec::<PathBuf>::new());
        assert!(!plain.upload.file_drop && !plain.uploads());
        let Command::Connect(c) = options(&format!(
            "connect {base} --control --send /home/u/a.bin --send /home/u/données.txt"
        ))
        .unwrap()
        .command
        else {
            unreachable!("connection required");
        };
        assert_eq!(
            c.upload.paths,
            [
                PathBuf::from("/home/u/a.bin"),
                PathBuf::from("/home/u/données.txt")
            ]
        );
        // The clipboard and file lanes share one controlled session.
        let Command::Connect(both) =
            options(&format!("connect {base} --control --clipboard --send /a"))
                .unwrap()
                .command
        else {
            unreachable!("connection required");
        };
        assert!(both.clipboard);
        assert_eq!(both.upload.paths, [PathBuf::from("/a")]);
        for (flags, code) in [
            ("--view-only --send /a", "send_requires_control"),
            (
                "--control --send /1 --send /2 --send /3 --send /4 --send /5 --send /6 --send /7 --send /8 --send /9",
                "send_too_many",
            ),
            // Absolute, no parent components, never valueless.
            ("--control --send relative.txt", "invalid_arguments"),
            ("--control --send /a/../b", "invalid_arguments"),
            ("--control --send", "invalid_arguments"),
        ] {
            assert_eq!(
                options(&format!("connect {base} {flags}"))
                    .err()
                    .unwrap()
                    .code,
                code,
                "{flags}"
            );
        }
        for refused in [
            "displays n-host --experimental-native --send /a",
            "hosts --send /a",
        ] {
            assert_eq!(options(refused).err().unwrap().code, "invalid_arguments");
        }
    }
    #[test]
    fn file_drop_explicitly_enables_upload_without_selecting_a_source() {
        let base = "connect n-host --experimental-native --worker /opt/fr/worker --display only";
        for extra in ["", "--receive /private/downloads", "--audio --no-clipboard"] {
            let Command::Connect(connection) =
                options(&format!("{base} --control --file-drop {extra}"))
                    .unwrap()
                    .command
            else {
                unreachable!("connection required");
            };
            assert!(connection.upload.file_drop && connection.uploads());
            assert!(
                connection.upload.paths.is_empty(),
                "drop consent is not a selected path"
            );
        }
        let Command::Connect(connection) = options(&format!(
            "{base} --control --file-drop --send /private/initial"
        ))
        .unwrap()
        .command
        else {
            unreachable!("connection required");
        };
        assert!(connection.upload.file_drop && connection.uploads());
        assert_eq!(connection.upload.paths, [PathBuf::from("/private/initial")]);
        for (arguments, code) in [
            (
                format!("{base} --view-only --file-drop"),
                "file_drop_requires_control",
            ),
            (
                format!("{base} --control --file-drop --file-drop"),
                "invalid_arguments",
            ),
            (
                "displays n-host --experimental-native --file-drop".to_owned(),
                "invalid_arguments",
            ),
            ("hosts --file-drop".to_owned(), "invalid_arguments"),
        ] {
            assert_eq!(options(&arguments).err().unwrap().code, code, "{arguments}");
        }
    }
    #[test]
    fn receive_requires_one_explicit_control_only_destination() {
        let base = "n-host --experimental-native --worker /opt/fr/worker --display only";
        let Command::Connect(plain) = options(&format!("connect {base} --control"))
            .unwrap()
            .command
        else {
            unreachable!("connection required");
        };
        assert!(plain.receive.is_none());
        let Command::Connect(selected) = options(&format!(
            "connect {base} --control --clipboard --audio --receive /home/u/downloads"
        ))
        .unwrap()
        .command
        else {
            unreachable!("connection required");
        };
        assert_eq!(
            selected
                .receive
                .as_ref()
                .map(|request| request.directory.as_path()),
            Some(std::path::Path::new("/home/u/downloads"))
        );
        assert!(selected.control && selected.clipboard && selected.audio.is_some());
        assert_eq!(selected.upload.paths, [] as [PathBuf; 0]);
        for (flags, code) in [
            ("--view-only --receive /a", "receive_requires_control"),
            ("--control --receive relative", "invalid_arguments"),
            ("--control --receive /a/../b", "invalid_arguments"),
            ("--control --receive", "invalid_arguments"),
            ("--control --receive /a --receive /b", "invalid_arguments"),
        ] {
            assert_eq!(
                options(&format!("connect {base} {flags}"))
                    .err()
                    .unwrap()
                    .code,
                code,
                "{flags}"
            );
        }
        for flags in [
            "--control --send /a --receive /b",
            "--control --receive /b --send /a",
        ] {
            let Command::Connect(both) =
                options(&format!("connect {base} {flags}")).unwrap().command
            else {
                panic!("controlled connection");
            };
            assert_eq!(both.upload.paths, [PathBuf::from("/a")]);
            assert_eq!(
                both.receive.as_ref().unwrap().directory,
                PathBuf::from("/b")
            );
        }
        for refused in [
            "displays n-host --experimental-native --receive /a",
            "hosts --receive /a",
            "doctor --receive /a",
        ] {
            assert_eq!(options(refused).err().unwrap().code, "invalid_arguments");
        }
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
            "connection_role_required"
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

#[cfg(test)]
mod controlled_audio_tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn explicit_audio_and_files_preserve_the_independent_clipboard_choice() {
        let base = "connect n-host --control --experimental-native --display only --worker /opt/fr/fr-media-worker";
        for extra in [
            "--audio",
            "--audio --clipboard",
            "--audio --no-clipboard",
            "--audio --send /a",
            "--audio --clipboard --send /a",
            "--audio --no-clipboard --send /a",
        ] {
            let Command::Connect(connection) =
                parse(&args(&format!("{base} {extra}"))).unwrap().command
            else {
                unreachable!("connection required");
            };
            assert!(connection.control && connection.audio.is_some());
            assert_eq!(connection.clipboard, !extra.contains("--no-clipboard"));
            assert_eq!(
                connection.upload.paths.len(),
                usize::from(extra.contains("--send"))
            );
        }
    }

    #[test]
    fn audio_cannot_bypass_role_transport_or_argument_guards() {
        for (text, code) in [
            ("connect n-host --audio", "connection_role_required"),
            (
                "connect n-host --control --view-only --audio",
                "connection_role_required",
            ),
            (
                "connect n-host --control --audio",
                "native_transport_unqualified",
            ),
            (
                "connect n-host --control --audio --audio",
                "invalid_arguments",
            ),
            (
                "connect n-host --control --audio-server /run/native",
                "invalid_arguments",
            ),
            (
                "connect n-host --control --audio --audio-server relative",
                "invalid_arguments",
            ),
        ] {
            assert_eq!(parse(&args(text)).err().unwrap().code, code, "{text}");
        }
    }
}

#[cfg(test)]
#[path = "options/saved_hosts_tests.rs"]
mod saved_hosts_tests;

#[cfg(test)]
#[path = "options/probe_tests.rs"]
mod probe_tests;
