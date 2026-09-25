//! Fixed binary frames: no text, JSON or firewall syntax from the caller.
//! Request: u16 big-endian body length (1..=`MAX_REQUEST`), then the body
//! `[VERSION, op, ...]`. Response: u32 big-endian length (1..=`MAX_RESPONSE`),
//! then `[VERSION, kind, ...]`. Every length is exact; trailing bytes refuse.
use super::{Protocols, interface_valid};
use std::{fmt, net::IpAddr};

pub const VERSION: u8 = 1;
/// Largest request body (an IPv6 install is 38 bytes).
pub const MAX_REQUEST: usize = 64;
/// Largest response body: one bounded nft read-back plus a small header.
pub const MAX_RESPONSE: usize = 16 * 1024 + 128;
const INSTALL: u8 = 1;
const RENEW: u8 = 2;
const REMOVE: u8 = 3;
const INSTALLED: u8 = 1;
const RENEWED: u8 = 2;
const REMOVED: u8 = 3;
const REFUSED: u8 = 4;

/// The only rule a caller can ask for. Its address is re-read from the kernel.
#[derive(Clone, PartialEq, Eq)]
pub struct Install {
    pub interface: String,
    pub address: IpAddr,
    pub port: u16,
    pub protocols: Protocols,
}
impl fmt::Debug for Install {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Install([local destination])")
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Install(Install),
    Renew { generation: u64 },
    Remove { generation: u64 },
}
#[derive(Clone, PartialEq, Eq)]
pub enum Response {
    /// `readback` is the helper's own `nft -j -n list table` of `table`.
    Installed {
        generation: u64,
        index: u32,
        table: String,
        readback: Vec<u8>,
    },
    Renewed {
        generation: u64,
        readback: Vec<u8>,
    },
    Removed {
        generation: u64,
    },
    Refused(Refusal),
}
impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed { generation, .. } => write!(f, "Installed({generation})"),
            Self::Renewed { generation, .. } => write!(f, "Renewed({generation})"),
            Self::Removed { generation } => write!(f, "Removed({generation})"),
            Self::Refused(reason) => write!(f, "Refused({reason:?})"),
        }
    }
}

/// Typed helper refusal. No command output, address or name is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum Refusal {
    Malformed = 1,
    UnsupportedVersion = 2,
    UnknownOperation = 3,
    Oversized = 4,
    PeerNotAllowed = 5,
    CapacityExhausted = 6,
    RateLimited = 7,
    InterfaceNotConfigured = 8,
    InterfaceUnqualified = 9,
    AddressNotAssigned = 10,
    InvalidAddress = 11,
    InvalidPort = 12,
    InvalidProtocols = 13,
    AlreadyInstalled = 14,
    NotInstalled = 15,
    StaleGeneration = 16,
    ForeignTable = 17,
    FirewallFailed = 18,
    FirewallMismatch = 19,
    InterfaceChanged = 20,
}
impl Refusal {
    pub(super) const ALL: [Self; 20] = [
        Self::Malformed,
        Self::UnsupportedVersion,
        Self::UnknownOperation,
        Self::Oversized,
        Self::PeerNotAllowed,
        Self::CapacityExhausted,
        Self::RateLimited,
        Self::InterfaceNotConfigured,
        Self::InterfaceUnqualified,
        Self::AddressNotAssigned,
        Self::InvalidAddress,
        Self::InvalidPort,
        Self::InvalidProtocols,
        Self::AlreadyInstalled,
        Self::NotInstalled,
        Self::StaleGeneration,
        Self::ForeignTable,
        Self::FirewallFailed,
        Self::FirewallMismatch,
        Self::InterfaceChanged,
    ];
    pub const fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.code() == code)
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed_request",
            Self::UnsupportedVersion => "unsupported_version",
            Self::UnknownOperation => "unknown_operation",
            Self::Oversized => "oversized_request",
            Self::PeerNotAllowed => "peer_not_allowed",
            Self::CapacityExhausted => "capacity_exhausted",
            Self::RateLimited => "rate_limited",
            Self::InterfaceNotConfigured => "interface_not_configured",
            Self::InterfaceUnqualified => "interface_unqualified",
            Self::AddressNotAssigned => "address_not_assigned",
            Self::InvalidAddress => "invalid_address",
            Self::InvalidPort => "invalid_port",
            Self::InvalidProtocols => "invalid_protocols",
            Self::AlreadyInstalled => "already_installed",
            Self::NotInstalled => "not_installed",
            Self::StaleGeneration => "stale_generation",
            Self::ForeignTable => "foreign_table",
            Self::FirewallFailed => "firewall_failed",
            Self::FirewallMismatch => "firewall_mismatch",
            Self::InterfaceChanged => "interface_changed",
        }
    }
}

/// Frame one request. Values the decoder refuses (an over-long interface name)
/// are encoded as-is so the helper, not the caller, decides.
pub fn encode_request(request: &Request) -> Vec<u8> {
    let mut body = vec![VERSION];
    match request {
        Request::Install(install) => {
            body.extend([INSTALL, install.protocols.bits()]);
            body.extend(install.port.to_be_bytes());
            match install.address {
                IpAddr::V4(v4) => {
                    body.push(4);
                    body.extend(v4.octets());
                }
                IpAddr::V6(v6) => {
                    body.push(6);
                    body.extend(v6.octets());
                }
            }
            body.push(u8::try_from(install.interface.len()).unwrap_or(u8::MAX));
            body.extend(install.interface.as_bytes());
        }
        Request::Renew { generation } => {
            body.push(RENEW);
            body.extend(generation.to_be_bytes());
        }
        Request::Remove { generation } => {
            body.push(REMOVE);
            body.extend(generation.to_be_bytes());
        }
    }
    let mut frame = u16::try_from(body.len())
        .unwrap_or(u16::MAX)
        .to_be_bytes()
        .to_vec();
    frame.extend(body);
    frame
}

/// Decode one request body (the length prefix was already bounded).
pub fn decode_request(body: &[u8]) -> Result<Request, Refusal> {
    if body.len() > MAX_REQUEST {
        return Err(Refusal::Oversized);
    }
    let [version, op, rest @ ..] = body else {
        return Err(Refusal::Malformed);
    };
    if *version != VERSION {
        return Err(Refusal::UnsupportedVersion);
    }
    match *op {
        INSTALL => {
            let [bits, high, low, family, rest @ ..] = rest else {
                return Err(Refusal::Malformed);
            };
            let protocols = Protocols::from_bits(*bits).ok_or(Refusal::InvalidProtocols)?;
            let (address, rest) = match (*family, rest) {
                (4, [a, b, c, d, rest @ ..]) => (IpAddr::from([*a, *b, *c, *d]), rest),
                (6, rest) if rest.len() >= 16 => {
                    let (octets, rest) = rest.split_at(16);
                    let octets: [u8; 16] = octets.try_into().map_err(|_| Refusal::Malformed)?;
                    (IpAddr::from(octets), rest)
                }
                _ => return Err(Refusal::Malformed),
            };
            let [length, name @ ..] = rest else {
                return Err(Refusal::Malformed);
            };
            let interface = std::str::from_utf8(name).map_err(|_| Refusal::Malformed)?;
            if usize::from(*length) != name.len() || !interface_valid(interface) {
                return Err(Refusal::Malformed);
            }
            Ok(Request::Install(Install {
                interface: interface.to_owned(),
                address,
                port: u16::from_be_bytes([*high, *low]),
                protocols,
            }))
        }
        RENEW | REMOVE => {
            let generation = u64::from_be_bytes(rest.try_into().map_err(|_| Refusal::Malformed)?);
            Ok(if *op == RENEW {
                Request::Renew { generation }
            } else {
                Request::Remove { generation }
            })
        }
        _ => Err(Refusal::UnknownOperation),
    }
}

pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut body = vec![VERSION];
    match response {
        Response::Installed {
            generation,
            index,
            table,
            readback,
        } => {
            body.push(INSTALLED);
            body.extend(generation.to_be_bytes());
            body.extend(index.to_be_bytes());
            body.push(u8::try_from(table.len()).unwrap_or(u8::MAX));
            body.extend(table.as_bytes());
            body.extend(readback);
        }
        Response::Renewed {
            generation,
            readback,
        } => {
            body.push(RENEWED);
            body.extend(generation.to_be_bytes());
            body.extend(readback);
        }
        Response::Removed { generation } => {
            body.push(REMOVED);
            body.extend(generation.to_be_bytes());
        }
        Response::Refused(reason) => body.extend([REFUSED, reason.code()]),
    }
    let mut frame = u32::try_from(body.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    frame.extend(body);
    frame
}

/// Decode one response body. Read-backs are bounded here and validated by the
/// broker's own `validate_rule`, never trusted merely for parsing.
pub fn decode_response(body: &[u8]) -> Result<Response, Refusal> {
    if body.len() > MAX_RESPONSE {
        return Err(Refusal::Oversized);
    }
    let [version, kind, rest @ ..] = body else {
        return Err(Refusal::Malformed);
    };
    if *version != VERSION {
        return Err(Refusal::UnsupportedVersion);
    }
    let generation = |bytes: &[u8]| -> Result<u64, Refusal> {
        Ok(u64::from_be_bytes(
            bytes
                .get(..8)
                .and_then(|b| b.try_into().ok())
                .ok_or(Refusal::Malformed)?,
        ))
    };
    match *kind {
        INSTALLED => {
            let generation = generation(rest)?;
            let rest = &rest[8..];
            let index = u32::from_be_bytes(
                rest.get(..4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(Refusal::Malformed)?,
            );
            let [length, rest @ ..] = &rest[4..] else {
                return Err(Refusal::Malformed);
            };
            let (table, readback) = rest
                .split_at_checked(usize::from(*length))
                .ok_or(Refusal::Malformed)?;
            let table = std::str::from_utf8(table).map_err(|_| Refusal::Malformed)?;
            super::owned_table(table)?;
            Ok(Response::Installed {
                generation,
                index,
                table: table.to_owned(),
                readback: readback.to_vec(),
            })
        }
        RENEWED => Ok(Response::Renewed {
            generation: generation(rest)?,
            readback: rest[8..].to_vec(),
        }),
        REMOVED if rest.len() == 8 => Ok(Response::Removed {
            generation: generation(rest)?,
        }),
        REFUSED => match rest {
            [code] => Refusal::from_code(*code)
                .map(Response::Refused)
                .ok_or(Refusal::Malformed),
            _ => Err(Refusal::Malformed),
        },
        REMOVED => Err(Refusal::Malformed),
        _ => Err(Refusal::UnknownOperation),
    }
}
