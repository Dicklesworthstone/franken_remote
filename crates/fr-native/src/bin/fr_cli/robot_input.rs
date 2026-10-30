//! Explicit local automation through the original foreground control owner.
//! Commands contain comparison handles only; host tickets never leave that owner.
use crate::{Failure, options};
#[cfg(any(target_os = "linux", test))]
use fr_client::robot::{InputDisposition, input::native::Report};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
#[path = "robot_input/client.rs"]
mod client;

pub(crate) struct Output {
    pub text: String,
    pub exit: u8,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Scroll { x: i32, y: i32, dx: i32, dy: i32 },
    Click { button: u8, x: i32, y: i32 },
    KeyTap { usage: u16 },
}
impl Operation {
    #[cfg(any(target_os = "linux", test))]
    pub fn count(self) -> u8 {
        if matches!(self, Self::Scroll { .. }) {
            1
        } else {
            2
        }
    }
    pub fn valid(self) -> bool {
        match self {
            Self::Scroll { dx, dy, .. } => {
                (-120..=120).contains(&dx) && (-120..=120).contains(&dy) && (dx != 0 || dy != 0)
            }
            Self::Click { button, .. } => (1..=5).contains(&button),
            Self::KeyTap { usage } => fr_core::input::PhysicalKey::new(usage).is_some(),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// Keep the explicit request_id spelling in the local JSON contract.
#[allow(clippy::struct_field_names)]
pub(crate) struct Request {
    pub lease: [u8; 16],
    pub request_id: u64,
    pub observation_id: u64,
    pub max_observation_age_ms: u32,
    pub operation: Operation,
}
impl Request {
    pub fn valid(self) -> bool {
        self.lease != [0; 16]
            && self.request_id != 0
            && self.observation_id != 0
            && (1..=1000).contains(&self.max_observation_age_ms)
            && self.operation.valid()
    }
}
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    Unavailable,
    InvalidRequest,
    LeaseMismatch,
    ObservationUnknown,
    ObservationExpired,
    ViewChanged,
    Busy,
    RequestChanged,
    Consumed,
    InputUnavailable,
    Stopped,
}
#[cfg(any(target_os = "linux", test))]
impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "robot_input_unavailable",
            Self::InvalidRequest => "invalid_robot_input",
            Self::LeaseMismatch => "robot_lease_mismatch",
            Self::ObservationUnknown => "robot_observation_unknown",
            Self::ObservationExpired => "robot_observation_expired",
            Self::ViewChanged => "robot_view_changed",
            Self::Busy => "robot_input_busy",
            Self::RequestChanged => "robot_request_changed",
            Self::Consumed => "robot_request_consumed",
            Self::InputUnavailable => "robot_input_not_ready",
            Self::Stopped => "robot_owner_stopped",
        }
    }
}
/// One explicitly requested observation; its identity pins the original source
/// age and ALL input generations. Reading status never refreshes an old action.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub ready: bool,
    pub lease: Option<String>,
    pub observation_id: Option<u64>,
    pub next_request_id: Option<u64>,
    pub geometry_generation: Option<u64>,
    pub viewport_generation: Option<u64>,
    pub configuration_generation: Option<u64>,
    pub recovery_generation: Option<u64>,
    pub frame: Option<u64>,
    pub source_age_upper_ms: Option<u64>,
    pub physical_visibility_proven: bool,
}
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Response {
    Status { snapshot: Snapshot },
    Result { report: Report },
    Refused { reason: Refusal },
}
#[cfg(any(target_os = "linux", test))]
impl Response {
    pub fn render(self, json: bool) -> Result<Output, Failure> {
        let (outcome, stage, error, data, exit) = match self {
            Self::Status { snapshot } => (
                "success",
                Some("observed"),
                None,
                serde_json::to_value(snapshot),
                0,
            ),
            Self::Result { report } => {
                report.validate().map_err(|_| protocol())?;
                let (outcome, exit) = match report.disposition {
                    InputDisposition::Committed => ("success", 0),
                    InputDisposition::Partial => ("partial_submission", 1),
                    InputDisposition::UnknownEffect => ("unknown_external_effect", 1),
                    InputDisposition::Refused => ("refusal", 1),
                };
                let stage = report.stage.map(|stage| match stage {
                    fr_client::robot::AcknowledgementStage::Admitted => "admitted",
                    fr_client::robot::AcknowledgementStage::SubmittedToOs => "submitted_to_os",
                    fr_client::robot::AcknowledgementStage::Observed => "observed",
                });
                (outcome, stage, None, serde_json::to_value(report), exit)
            }
            Self::Refused { reason } => (
                "refusal",
                None,
                Some(reason.code()),
                Ok(serde_json::Value::Null),
                1,
            ),
        };
        let data = data.map_err(|_| protocol())?;
        let value = serde_json::json!({
            "schema_version": 1, "timestamp_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().and_then(|v| u64::try_from(v.as_millis()).ok()),
            "outcome": outcome, "stage": stage, "error": error.map(|code| serde_json::json!({"code":code,"next_action":"Inspect the original owner and retained request result. A new observation does not authorize replay of an uncertain action."})),
            "data":data,
        });
        let text = if json {
            serde_json::to_string(&value)
        } else {
            serde_json::to_string_pretty(&value)
        }
        .map_err(|_| protocol())?;
        Ok(Output { text, exit })
    }
}
#[cfg(any(target_os = "linux", test))]
pub(crate) fn protocol() -> Failure {
    Failure::new(
        "local_session_protocol",
        "The authenticated local owner returned an invalid bounded robot response. Do not repeat an uncertain input action.",
        1,
    )
}
fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Run fr --help. Explicit robot input needs the original connection, lease, observation, monotonic request ID and one bounded action.",
        2,
    )
}
pub(super) fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|a| a == "robot")
        && (args.get(1).is_some_and(|a| a == "input")
            && args.iter().any(|a| a == "--connection-id")
            || args
                .get(1..3)
                .is_some_and(|a| a[0] == "session" && a[1] == "open")
                && args.iter().any(|a| a == "--accept-input"))
}
pub(crate) fn id(text: &str) -> Result<[u8; 16], Failure> {
    if text.len() != 32
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(usage());
    }
    let mut id = [0; 16];
    for (byte, part) in id.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        let part = std::str::from_utf8(part).map_err(|_| usage())?;
        *byte = u8::from_str_radix(part, 16).map_err(|_| usage())?;
    }
    if id == [0; 16] {
        return Err(usage());
    }
    Ok(id)
}
// Unsupported platforms still validate the complete request before refusing.
// Only the Linux owner consumes the validated fields.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) struct Command {
    pub target: String,
    pub connection: String,
    pub request: Option<Request>,
    pub json: bool,
}
fn bounded(args: &[String]) -> Result<(), Failure> {
    if args.len() > 32
        || args
            .iter()
            .any(|a| a.len() > 4096 || a.chars().any(char::is_control))
    {
        return Err(usage());
    }
    Ok(())
}
fn opening(args: &[String]) -> Result<options::Options, Failure> {
    bounded(args)?;
    if args.len() < 8
        || args[..3] != ["robot", "session", "open"]
        || args[4..8] != ["--foreground", "--role", "control", "--accept-input"]
    {
        return Err(usage());
    }
    let mut translated = vec![
        "connect".into(),
        args[3].clone(),
        "--control".into(),
        "--no-clipboard".into(),
        "--attempts".into(),
        "1".into(),
    ];
    let mut i = 8;
    let mut seen = std::collections::BTreeSet::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--experimental-native" | "--by-name" | "--ipv6" | "--json" => {
                translated.push(flag.into());
            }
            "--display" | "--worker" | "--trust-roots" | "--x-display" | "--socket" | "--port"
            | "--fit" => {
                let value = args.get(i).ok_or_else(usage)?;
                i += 1;
                translated.extend([flag.into(), value.clone()]);
            }
            _ => return Err(usage()),
        }
    }
    let parsed = options::parse(&translated)?;
    let options::Command::Connect(connection) = &parsed.command else {
        return Err(usage());
    };
    if connection.display == options::DisplayChoice::Choose {
        return Err(usage());
    }
    Ok(parsed)
}
fn command(args: &[String]) -> Result<Command, Failure> {
    bounded(args)?;
    if args.get(0..2).is_none_or(|a| a != ["robot", "input"]) {
        return Err(usage());
    }
    let target = args
        .get(2)
        .filter(|a| !a.is_empty() && a.len() <= 253 && !a.starts_with("--"))
        .ok_or_else(usage)?
        .clone();
    let mut connection = None;
    let mut lease = None;
    let mut request_id = None;
    let mut observation_id = None;
    let mut max_age = None;
    let mut operation = None;
    let mut status = false;
    let mut json = false;
    let mut seen = std::collections::BTreeSet::new();
    let mut i = 3;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        if flag == "--json" {
            json = true;
            continue;
        }
        if flag == "--status" {
            status = true;
            continue;
        }
        let value = args.get(i).ok_or_else(usage)?;
        i += 1;
        match flag {
            "--connection-id" => {
                id(value)?;
                connection = Some(value.clone());
            }
            "--lease" => lease = Some(id(value)?),
            "--request-id" => request_id = Some(value.parse().map_err(|_| usage())?),
            "--observation-id" => observation_id = Some(value.parse().map_err(|_| usage())?),
            "--max-observation-age-ms" => max_age = Some(value.parse().map_err(|_| usage())?),
            "--scroll" | "--click" | "--key" => {
                if operation.is_some() {
                    return Err(usage());
                }
                let values = value
                    .split(',')
                    .map(str::parse::<i32>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| usage())?;
                operation = Some(match (flag, values.as_slice()) {
                    ("--scroll", &[x, y, dx, dy]) => Operation::Scroll { x, y, dx, dy },
                    ("--click", &[button, x, y]) => Operation::Click {
                        button: u8::try_from(button).map_err(|_| usage())?,
                        x,
                        y,
                    },
                    ("--key", &[usage_value]) => Operation::KeyTap {
                        usage: u16::try_from(usage_value).map_err(|_| usage())?,
                    },
                    _ => return Err(usage()),
                });
            }
            _ => return Err(usage()),
        }
    }
    let request = if status {
        if lease.is_some()
            || request_id.is_some()
            || observation_id.is_some()
            || max_age.is_some()
            || operation.is_some()
        {
            return Err(usage());
        }
        None
    } else {
        let request = Request {
            lease: lease.ok_or_else(usage)?,
            request_id: request_id.ok_or_else(usage)?,
            observation_id: observation_id.ok_or_else(usage)?,
            max_observation_age_ms: max_age.ok_or_else(usage)?,
            operation: operation.ok_or_else(usage)?,
        };
        if !request.valid() {
            return Err(usage());
        }
        Some(request)
    };
    Ok(Command {
        target,
        connection: connection.ok_or_else(usage)?,
        request,
        json,
    })
}
pub(super) fn run(args: &[String]) -> Result<Output, Failure> {
    if args.iter().any(|a| a == "--accept-input") {
        let parsed = opening(args)?;
        #[cfg(target_os = "linux")]
        {
            return crate::linux::run_automation(&parsed).map(|text| Output { text, exit: 0 });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = parsed;
            return Err(Failure::new(
                "platform_unavailable",
                "Foreground robot control requires the qualified Linux desktop path.",
                2,
            ));
        }
    }
    let command = command(args)?;
    #[cfg(target_os = "linux")]
    {
        client::run(&command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        Err(Failure::new(
            "platform_unavailable",
            "Authenticated foreground robot control is available on Linux.",
            2,
        ))
    }
}

#[cfg(test)]
#[path = "robot_input/tests.rs"]
mod tests;
