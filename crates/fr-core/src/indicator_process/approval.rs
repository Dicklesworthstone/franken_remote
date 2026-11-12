//! Bounded display-only attribution for one original local approval request.
//! The authenticated parent supplies labels from its live admission proof. The
//! child checks exact IPC correlation before opening; these bytes grant nothing.
use super::{Error, Frame, Kind};
use core::fmt::{self, Write as _};

pub const CONTEXT_BYTES: usize = 432;
pub const MAX_DETAILS_BYTES: usize = 1024;
const NODE_BYTES: usize = 128;
const NAME_BYTES: usize = 254;
const NODE_START: usize = 44;
const NAME_START: usize = NODE_START + NODE_BYTES;
const END: usize = NAME_START + NAME_BYTES;

/// Exact negotiated optional capabilities, not locally offered possibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features(u8);
impl Features {
    pub const NONE: Self = Self(0);
    pub const PLAYBACK: Self = Self(1);
    pub const CLIPBOARD: Self = Self(2);
    pub const FILE_SEND: Self = Self(4);
    pub const FILE_RECEIVE: Self = Self(8);
    pub const MICROPHONE: Self = Self(16);
    const ALL: u8 = 31;

    #[must_use]
    pub const fn with(self, feature: Self) -> Self {
        Self(self.0 | feature.0)
    }
    pub const fn contains(self, feature: Self) -> bool {
        self.0 & feature.0 == feature.0
    }
    fn valid(self, controlling: bool) -> bool {
        self.0 & !Self::ALL == 0 && (controlling || self.0 & !Self::PLAYBACK.0 == 0)
    }
}

/// An immutable rendering snapshot. No session nonce, key, endpoint or bearer
/// identifier belongs here. `local_request` is a separate local UI ordinal.
#[derive(Clone, PartialEq, Eq)]
pub struct Context {
    node: String,
    name: Option<String>,
    features: Features,
    local_request: u64,
}
impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovalContext([display-only attribution])")
    }
}
impl Context {
    pub fn new(
        node_id: &str,
        dns_name: Option<&str>,
        features: Features,
        local_request: u64,
    ) -> Result<Self, Error> {
        if node_id.is_empty()
            || node_id.len() > NODE_BYTES
            || !node_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            || dns_name.is_some_and(|name| !valid_name(name))
            || features.0 & !Features::ALL != 0
            || local_request == 0
        {
            return Err(Error);
        }
        Ok(Self {
            node: node_id.to_owned(),
            name: dns_name.map(str::to_owned),
            features,
            local_request,
        })
    }
    pub fn encode(&self, hello: Frame) -> Result<[u8; CONTEXT_BYTES], Error> {
        let controlling = controlling(hello)?;
        if !self.features.valid(controlling) {
            return Err(Error);
        }
        let mut out = [0; CONTEXT_BYTES];
        out[..4].copy_from_slice(b"FRAC");
        out[4] = 1;
        out[5] = hello.kind as u8;
        out[6] = self.features.0;
        out[8..16].copy_from_slice(&hello.sequence.to_be_bytes());
        out[16..32].copy_from_slice(&hello.epoch.to_be_bytes());
        out[32..40].copy_from_slice(&self.local_request.to_be_bytes());
        let name = self.name.as_deref().unwrap_or_default();
        out[40..42].copy_from_slice(
            &u16::try_from(self.node.len())
                .map_err(|_| Error)?
                .to_be_bytes(),
        );
        out[42..44].copy_from_slice(&u16::try_from(name.len()).map_err(|_| Error)?.to_be_bytes());
        out[NODE_START..NODE_START + self.node.len()].copy_from_slice(self.node.as_bytes());
        out[NAME_START..NAME_START + name.len()].copy_from_slice(name.as_bytes());
        Ok(out)
    }
    pub fn decode(hello: Frame, bytes: &[u8]) -> Result<Self, Error> {
        let controlling = controlling(hello)?;
        if bytes.len() != CONTEXT_BYTES
            || bytes[..4] != *b"FRAC"
            || bytes[4] != 1
            || bytes[5] != hello.kind as u8
            || bytes[7] != 0
            || bytes[8..16] != hello.sequence.to_be_bytes()
            || bytes[16..32] != hello.epoch.to_be_bytes()
            || bytes[END..].iter().any(|&b| b != 0)
        {
            return Err(Error);
        }
        let features = Features(bytes[6]);
        if !features.valid(controlling) {
            return Err(Error);
        }
        let local_request = u64::from_be_bytes(bytes[32..40].try_into().map_err(|_| Error)?);
        let node_len = usize::from(u16::from_be_bytes(
            bytes[40..42].try_into().map_err(|_| Error)?,
        ));
        let name_len = usize::from(u16::from_be_bytes(
            bytes[42..44].try_into().map_err(|_| Error)?,
        ));
        if node_len > NODE_BYTES || name_len > NAME_BYTES {
            return Err(Error);
        }
        let node = text(&bytes[NODE_START..NAME_START], node_len)?;
        let name = text(&bytes[NAME_START..END], name_len)?;
        Self::new(
            node,
            (!name.is_empty()).then_some(name),
            features,
            local_request,
        )
    }
    /// Complete printable ASCII, with explicit line breaks and no truncation.
    /// Native surfaces must also prove that every line fits before accepting.
    pub fn details(&self, hello: Frame) -> Result<String, Error> {
        let controlling = controlling(hello)?;
        if !self.features.valid(controlling) {
            return Err(Error);
        }
        let mut out = String::new();
        wrapped(&mut out, "Verified node: ", &self.node);
        wrapped(
            &mut out,
            "Daemon name: ",
            self.name.as_deref().unwrap_or("(unavailable)"),
        );
        writeln!(&mut out, "Local request: {}", self.local_request).map_err(|_| Error)?;
        out.push_str("Desktop: host login session; display selection follows approval.\n");
        out.push_str(if controlling {
            "Control: keyboard and pointer across the host login session.\n"
        } else {
            "Control: none; this request is read-only.\n"
        });
        out.push_str(if self.features.contains(Features::PLAYBACK) {
            "Audio: host playback, including apps on other displays.\n"
        } else {
            "Audio: no playback audio for this request.\n"
        });
        let on = |feature| {
            if self.features.contains(feature) {
                "on"
            } else {
                "off"
            }
        };
        writeln!(
            &mut out,
            "Clipboard: {}; file send to host: {}; file receive: {}",
            on(Features::CLIPBOARD),
            on(Features::FILE_SEND),
            on(Features::FILE_RECEIVE)
        )
        .map_err(|_| Error)?;
        out.push_str(if self.features.contains(Features::MICROPHONE) {
            "Microphone: separate explicit enable and local indicator required."
        } else {
            "Microphone: not enabled for this request."
        });
        if out.len() > MAX_DETAILS_BYTES
            || out.lines().count() > 14
            || out.lines().any(|line| line.len() > 78)
        {
            return Err(Error);
        }
        Ok(out)
    }
}

fn controlling(hello: Frame) -> Result<bool, Error> {
    if hello.sequence != 1 || hello.epoch == 0 {
        return Err(Error);
    }
    match hello.kind {
        Kind::ApproveViewDetails => Ok(false),
        Kind::ApproveControlDetails => Ok(true),
        _ => Err(Error),
    }
}
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_BYTES {
        return false;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    })
}
fn text(bytes: &[u8], len: usize) -> Result<&str, Error> {
    if bytes[len..].iter().any(|&b| b != 0) {
        return Err(Error);
    }
    core::str::from_utf8(&bytes[..len]).map_err(|_| Error)
}
fn wrapped(out: &mut String, prefix: &str, value: &str) {
    out.push_str(prefix);
    let mut remaining = value;
    let mut width = 78 - prefix.len();
    while remaining.len() > width {
        let (line, rest) = remaining.split_at(width);
        out.push_str(line);
        out.push_str("\n  ");
        remaining = rest;
        width = 76;
    }
    out.push_str(remaining);
    out.push('\n');
}

#[cfg(test)]
mod tests;
