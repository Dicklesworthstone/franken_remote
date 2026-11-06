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
    /// One absolute pointer state; no buttons, keys or scroll are submitted.
    Move {
        x: i32,
        y: i32,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    /// Bounded wheel scrolling while the canonical modifier set stays held.
    ModifiedScroll {
        modifiers: u8,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Click {
        button: u8,
        x: i32,
        y: i32,
    },
    /// A complete click while the canonical physical modifier set stays held.
    ModifiedClick {
        modifiers: u8,
        button: u8,
        x: i32,
        y: i32,
    },
    KeyTap {
        usage: u16,
    },
    /// USB modifiers 224..231 as a canonical set, followed by one physical key.
    KeyChord {
        modifiers: u8,
        usage: u16,
    },
    /// One press at the source, then a held move and release at the destination.
    Drag {
        button: u8,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    /// A complete drag while the canonical physical modifier set stays held.
    ModifiedDrag {
        modifiers: u8,
        button: u8,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
}
impl Operation {
    pub fn count(self) -> u8 {
        match self {
            Self::Move { .. } => 1,
            Self::Scroll { dx, dy, .. } => {
                // The public request spans up to 120 lines per axis, but one
                // native wheel action admits at most 32 total steps. A request
                // is at most eight original, receipt-gated actions. Invalid
                // raw IPC values remain non-panicking until valid() refuses.
                let lines = u64::from(dx.unsigned_abs()) + u64::from(dy.unsigned_abs());
                u8::try_from(
                    lines.div_ceil(u64::from(fr_core::input_submission::scroll::MAX_STEPS)),
                )
                .unwrap_or(u8::MAX)
            }
            Self::ModifiedScroll {
                modifiers,
                x,
                y,
                dx,
                dy,
            } => Self::Scroll { x, y, dx, dy }.count().saturating_add(
                u8::try_from(2 * modifiers.count_ones()).expect("bounded modifier set"),
            ),
            Self::Click { .. } | Self::KeyTap { .. } | Self::Drag { .. } => 2,
            Self::KeyChord { modifiers, .. }
            | Self::ModifiedClick { modifiers, .. }
            | Self::ModifiedDrag { modifiers, .. } => {
                // A byte has at most eight set bits, including invalid requests.
                u8::try_from(2 * (modifiers.count_ones() + 1)).expect("bounded modifier set")
            }
        }
    }
    pub fn valid(self) -> bool {
        match self {
            Self::Move { .. } => true,
            Self::Scroll { dx, dy, .. } => {
                (-120..=120).contains(&dx) && (-120..=120).contains(&dy) && (dx != 0 || dy != 0)
            }
            Self::ModifiedScroll {
                modifiers,
                x,
                y,
                dx,
                dy,
            } => {
                (1..=4).contains(&modifiers.count_ones())
                    && Self::Scroll { x, y, dx, dy }.valid()
                    && self.count() <= fr_client::robot::input::native::MAX_ACTIONS
            }
            Self::Click { button, .. } => (1..=5).contains(&button),
            Self::ModifiedClick {
                modifiers, button, ..
            } => (1..=4).contains(&modifiers.count_ones()) && (1..=5).contains(&button),
            Self::ModifiedDrag {
                modifiers,
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                (1..=4).contains(&modifiers.count_ones())
                    && (1..=5).contains(&button)
                    && (from_x, from_y) != (to_x, to_y)
            }
            Self::Drag {
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            } => (1..=5).contains(&button) && (from_x, from_y) != (to_x, to_y),
            Self::KeyTap { usage } => fr_core::input::PhysicalKey::new(usage).is_some(),
            Self::KeyChord { modifiers, usage } => {
                (1..=4).contains(&modifiers.count_ones())
                    && usage < 224
                    && fr_core::input::PhysicalKey::new(usage).is_some()
            }
        }
    }
    /// One bounded part of the original scroll. Preserve horizontal-then-
    /// vertical order, signs and exact distance without allocating or retaining
    /// an input queue. Later parts still need their original owner's gates.
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn scroll_lines(self, index: u8) -> Option<(i32, i32)> {
        let Self::Scroll { dx, dy, .. } = self else {
            return None;
        };
        if !self.valid() {
            return None;
        }
        let maximum = fr_core::input_submission::scroll::MAX_STEPS;
        let offset = u32::from(index) * maximum;
        let x = dx.unsigned_abs().saturating_sub(offset).min(maximum);
        let y = dy
            .unsigned_abs()
            .saturating_sub(offset.saturating_sub(dx.unsigned_abs()))
            .min(maximum - x);
        if x == 0 && y == 0 {
            return None;
        }
        Some((
            i32::try_from(x).ok()? * dx.signum(),
            i32::try_from(y).ok()? * dy.signum(),
        ))
    }
    /// Only the first and sole pointer state of an absolute move is encodable.
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn pointer(self, index: u8) -> Option<fr_core::input::DesktopPoint> {
        match self {
            Self::Move { x, y } if index == 0 => Some(fr_core::input::DesktopPoint { x, y }),
            _ => None,
        }
    }
    /// Both endpoints of the single bounded drag, in signed desktop pixels.
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn drag_points(
        self,
    ) -> Option<(fr_core::input::DesktopPoint, fr_core::input::DesktopPoint)> {
        match self {
            Self::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            }
            | Self::ModifiedDrag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => Some((
                fr_core::input::DesktopPoint {
                    x: from_x,
                    y: from_y,
                },
                fr_core::input::DesktopPoint { x: to_x, y: to_y },
            )),
            _ => None,
        }
    }
    /// Check the entire gesture before its first action can press a modifier.
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn within_bounds(self, bounds: fr_core::input::InputBounds) -> bool {
        if let Some((from, to)) = self.drag_points() {
            return bounds.contains(from) && bounds.contains(to);
        }
        match self {
            Self::Move { x, y }
            | Self::Scroll { x, y, .. }
            | Self::ModifiedScroll { x, y, .. }
            | Self::Click { x, y, .. }
            | Self::ModifiedClick { x, y, .. } => {
                bounds.contains(fr_core::input::DesktopPoint { x, y })
            }
            _ => true,
        }
    }
    /// Preflight all capabilities, including the position barrier of each
    /// button/scroll action. A supported modifier alone cannot admit a gesture.
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn supported(self, capabilities: fr_core::input_submission::Capabilities) -> bool {
        use fr_core::input_submission::{Capabilities, Capability};
        let required = match self {
            Self::Move { .. } => Capabilities::default().with(Capability::Absolute),
            Self::Scroll { .. } => Capabilities::default()
                .with(Capability::Absolute)
                .with(Capability::LineScroll),
            Self::ModifiedScroll { .. } => Capabilities::default()
                .with(Capability::Keys)
                .with(Capability::Absolute)
                .with(Capability::LineScroll),
            Self::Click { .. } | Self::Drag { .. } => Capabilities::default()
                .with(Capability::Absolute)
                .with(Capability::Buttons),
            Self::KeyTap { .. } | Self::KeyChord { .. } => {
                Capabilities::default().with(Capability::Keys)
            }
            Self::ModifiedClick { .. } | Self::ModifiedDrag { .. } => Capabilities::default()
                .with(Capability::Absolute)
                .with(Capability::Buttons)
                .with(Capability::Keys),
        };
        capabilities.contains_all(required)
    }
    fn parse(flag: &str, value: &str) -> Result<Self, Failure> {
        let values = value
            .split(',')
            .map(str::parse::<i32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| usage())?;
        Ok(match (flag, values.as_slice()) {
            ("--move", &[x, y]) => Self::Move { x, y },
            ("--scroll", &[x, y, dx, dy]) => Self::Scroll { x, y, dx, dy },
            ("--modified-scroll", [modifiers @ .., x, y, dx, dy]) => Self::ModifiedScroll {
                modifiers: Self::modifier_mask(modifiers)?,
                x: *x,
                y: *y,
                dx: *dx,
                dy: *dy,
            },
            ("--click", &[button, x, y]) => Self::Click {
                button: u8::try_from(button).map_err(|_| usage())?,
                x,
                y,
            },
            ("--drag", &[button, from_x, from_y, to_x, to_y]) => Self::Drag {
                button: u8::try_from(button).map_err(|_| usage())?,
                from_x,
                from_y,
                to_x,
                to_y,
            },
            ("--modified-click", [modifiers @ .., button, x, y]) => Self::ModifiedClick {
                modifiers: Self::modifier_mask(modifiers)?,
                button: u8::try_from(*button).map_err(|_| usage())?,
                x: *x,
                y: *y,
            },
            ("--modified-drag", [modifiers @ .., button, from_x, from_y, to_x, to_y]) => {
                Self::ModifiedDrag {
                    modifiers: Self::modifier_mask(modifiers)?,
                    button: u8::try_from(*button).map_err(|_| usage())?,
                    from_x: *from_x,
                    from_y: *from_y,
                    to_x: *to_x,
                    to_y: *to_y,
                }
            }
            ("--key", &[usage_value]) => Self::KeyTap {
                usage: u16::try_from(usage_value).map_err(|_| usage())?,
            },
            ("--chord", values) => Self::chord(values)?,
            _ => return Err(usage()),
        })
    }
    fn chord(values: &[i32]) -> Result<Self, Failure> {
        let [modifiers @ .., key] = values else {
            return Err(usage());
        };
        let operation = Self::KeyChord {
            modifiers: Self::modifier_mask(modifiers)?,
            usage: u16::try_from(*key).map_err(|_| usage())?,
        };
        if !operation.valid() {
            return Err(usage());
        }
        Ok(operation)
    }
    fn modifier_mask(modifiers: &[i32]) -> Result<u8, Failure> {
        if !(1..=4).contains(&modifiers.len()) {
            return Err(usage());
        }
        let mut mask = 0_u8;
        for modifier in modifiers {
            if !(224..=231).contains(modifier) {
                return Err(usage());
            }
            let bit = 1 << (*modifier - 224);
            if mask & bit != 0 {
                return Err(usage());
            }
            mask |= bit;
        }
        Ok(mask)
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
    OutOfBounds,
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
            Self::OutOfBounds => "robot_input_out_of_bounds",
        }
    }
}
/// The input capabilities used by the public bounded robot operations.
/// In a known support object, omitted capabilities are not granted.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InputCapability {
    PhysicalKeys,
    AbsolutePointer,
    Buttons,
    LineScroll,
}
/// Signed host desktop pixels, with exclusive right and bottom edges.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputRectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
/// Descriptive limits from the SAME pinned candidate as the input observation.
/// These values do not issue an input ticket or authorize any action.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputSupport {
    pub bounds: InputRectangle,
    pub capabilities: Vec<InputCapability>,
}
#[cfg(any(target_os = "linux", test))]
impl InputSupport {
    #[cfg(any(feature = "linux-desktop", test))]
    pub fn from_grant(
        bounds: fr_core::input::InputBounds,
        capabilities: fr_core::input_submission::Capabilities,
    ) -> Self {
        use fr_core::input_submission::Capability;
        Self {
            bounds: InputRectangle {
                x: bounds.origin().x,
                y: bounds.origin().y,
                width: bounds.width(),
                height: bounds.height(),
            },
            capabilities: [
                (InputCapability::PhysicalKeys, Capability::Keys),
                (InputCapability::AbsolutePointer, Capability::Absolute),
                (InputCapability::Buttons, Capability::Buttons),
                (InputCapability::LineScroll, Capability::LineScroll),
            ]
            .into_iter()
            .filter_map(|(name, capability)| capabilities.contains(capability).then_some(name))
            .collect(),
        }
    }
    pub fn valid(&self) -> bool {
        fr_core::input::InputBounds::new(
            fr_core::input::DesktopPoint {
                x: self.bounds.x,
                y: self.bounds.y,
            },
            self.bounds.width,
            self.bounds.height,
        )
        .is_some()
            && self.capabilities.len() <= 4
            && self.capabilities.windows(2).all(|pair| pair[0] < pair[1])
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
    /// Older local owners omit this field. Missing support is unknown, not full.
    #[serde(default)]
    pub input_support: Option<InputSupport>,
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
    if connection.choose_host || connection.display == options::DisplayChoice::Choose {
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
            "--move" | "--scroll" | "--click" | "--key" | "--chord" | "--drag"
            | "--modified-click" | "--modified-drag" | "--modified-scroll" => {
                if operation.is_some() {
                    return Err(usage());
                }
                operation = Some(Operation::parse(flag, value)?);
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
