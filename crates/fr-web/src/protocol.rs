//! Binary framing for browser first-message authentication, role-specific
//! auxiliary channel attachment, and honest transport profile labeling.
//!
//! Nonces and tickets travel ONLY inside bounded first binary frames after
//! exact-Origin HTTPS/WSS establishment — never in URLs or query strings.

use core::fmt;
use fr_core::ids::RemoteSessionId;
use fr_transport::wss::WssProfile;
use serde::{Deserialize, Serialize};

/// First-message authentication magic on `/ws/control` (matches `SocketAuthGuard`).
pub const AUTH_MAGIC: [u8; 4] = *b"FRBA";
/// First-message auxiliary attachment magic on `/ws/video` and `/ws/audio`.
pub const ATTACH_MAGIC: [u8; 4] = *b"FRAT";
/// Server-to-browser control frame issuing a single-use auxiliary channel ticket.
pub const TICKET_GRANT_MAGIC: [u8; 4] = *b"FRTK";

/// Exact byte length of a first-message control authentication frame (`FRBA` + 32B nonce).
pub const AUTH_MESSAGE_BYTES: usize = 4 + 32;
/// Exact byte length of an auxiliary channel attachment frame (`FRAT` + 16B session + 1B role + 32B ticket).
pub const ATTACH_MESSAGE_BYTES: usize = 4 + 16 + 1 + 32;
/// Exact byte length of a server-issued ticket grant frame (`FRTK` + 1B role + 16B session + 32B ticket).
pub const TICKET_GRANT_BYTES: usize = 4 + 1 + 16 + 32;

/// Distinct auxiliary browser channels attached with role-specific single-use tickets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum BrowserChannelKind {
    Video = 1,
    AudioDown = 2,
    AudioUp = 3,
    Clipboard = 4,
    Files = 5,
    Diagnostics = 6,
}

impl BrowserChannelKind {
    /// Parse a single wire role discriminant byte.
    pub const fn from_byte(byte: u8) -> Result<Self, BrowserProtocolError> {
        match byte {
            1 => Ok(Self::Video),
            2 => Ok(Self::AudioDown),
            3 => Ok(Self::AudioUp),
            4 => Ok(Self::Clipboard),
            5 => Ok(Self::Files),
            6 => Ok(Self::Diagnostics),
            _ => Err(BrowserProtocolError::InvalidChannelRole),
        }
    }
}

/// Honest transport profile active for the browser workstation session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserTransportProfile {
    /// Qualified HTTP/3 `WebTransport` with datagrams and streams.
    WebTransportHttp3,
    /// Explicit degraded compatibility profile over WSS (TCP head-of-line applies).
    WssDegraded {
        multiplexed: bool,
        tcp_head_of_line: bool,
    },
}

impl BrowserTransportProfile {
    /// Construct an honest degraded WSS profile from `fr_transport::wss::WssProfile`.
    #[must_use]
    pub const fn from_wss(profile: WssProfile) -> Self {
        Self::WssDegraded {
            multiplexed: profile.multiplexed_transport,
            tcp_head_of_line: profile.tcp_head_of_line,
        }
    }

    /// Stable machine-readable transport diagnostic label.
    #[must_use]
    pub const fn diagnostic_label(self) -> &'static str {
        match self {
            Self::WebTransportHttp3 => "webtransport_h3",
            Self::WssDegraded {
                multiplexed: true, ..
            } => "wss_degraded_tcp_hol_multiplexed",
            Self::WssDegraded {
                multiplexed: false, ..
            } => "wss_degraded_tcp_hol_dedicated",
        }
    }

    /// True if the browser is operating on the explicit degraded WSS fallback.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::WssDegraded { .. })
    }
}

/// Typed protocol framing errors for browser bootstrap/attachment messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserProtocolError {
    MalformedMagic,
    InvalidLength,
    InvalidChannelRole,
    ZeroSessionId,
}

impl fmt::Display for BrowserProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MalformedMagic => "malformed browser control frame magic",
            Self::InvalidLength => "invalid browser control frame length",
            Self::InvalidChannelRole => "unknown browser auxiliary channel role",
            Self::ZeroSessionId => "zero session ID rejected",
        })
    }
}

impl core::error::Error for BrowserProtocolError {}

/// Encode `FRBA` + 32-byte single-use bootstrap nonce for the first `/ws/control` frame.
#[must_use]
pub fn encode_auth_message(nonce: &[u8; 32]) -> [u8; AUTH_MESSAGE_BYTES] {
    let mut out = [0u8; AUTH_MESSAGE_BYTES];
    out[..4].copy_from_slice(&AUTH_MAGIC);
    out[4..36].copy_from_slice(nonce);
    out
}

/// Parse a `FRBA` first-message authentication frame into its raw 32-byte nonce.
pub fn parse_auth_message(bytes: &[u8]) -> Result<[u8; 32], BrowserProtocolError> {
    if bytes.len() != AUTH_MESSAGE_BYTES {
        return Err(BrowserProtocolError::InvalidLength);
    }
    if bytes[..4] != AUTH_MAGIC {
        return Err(BrowserProtocolError::MalformedMagic);
    }
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&bytes[4..36]);
    Ok(nonce)
}

/// Encode `FRAT` + 16B `session_id` + 1B `role` + 32B single-use ticket for auxiliary attach.
pub fn encode_attach_message(
    session_id: RemoteSessionId,
    role: BrowserChannelKind,
    ticket: &[u8; 32],
) -> Result<[u8; ATTACH_MESSAGE_BYTES], BrowserProtocolError> {
    if session_id.as_raw() == 0 {
        return Err(BrowserProtocolError::ZeroSessionId);
    }
    let mut out = [0u8; ATTACH_MESSAGE_BYTES];
    out[..4].copy_from_slice(&ATTACH_MAGIC);
    out[4..20].copy_from_slice(&session_id.as_raw().to_be_bytes());
    out[20] = role as u8;
    out[21..53].copy_from_slice(ticket);
    Ok(out)
}

/// Parse a `FRAT` auxiliary channel attachment frame.
pub fn parse_attach_message(
    bytes: &[u8],
) -> Result<(RemoteSessionId, BrowserChannelKind, [u8; 32]), BrowserProtocolError> {
    if bytes.len() != ATTACH_MESSAGE_BYTES {
        return Err(BrowserProtocolError::InvalidLength);
    }
    if bytes[..4] != ATTACH_MAGIC {
        return Err(BrowserProtocolError::MalformedMagic);
    }
    let mut sid_bytes = [0u8; 16];
    sid_bytes.copy_from_slice(&bytes[4..20]);
    let session_id = RemoteSessionId::from_raw(u128::from_be_bytes(sid_bytes));
    if session_id.as_raw() == 0 {
        return Err(BrowserProtocolError::ZeroSessionId);
    }
    let role = BrowserChannelKind::from_byte(bytes[20])?;
    let mut ticket = [0u8; 32];
    ticket.copy_from_slice(&bytes[21..53]);
    Ok((session_id, role, ticket))
}

/// Encode server-to-browser `FRTK` auxiliary ticket grant frame sent over authenticated `/ws/control`.
pub fn encode_ticket_grant(
    session_id: RemoteSessionId,
    role: BrowserChannelKind,
    ticket: &[u8; 32],
) -> Result<[u8; TICKET_GRANT_BYTES], BrowserProtocolError> {
    if session_id.as_raw() == 0 {
        return Err(BrowserProtocolError::ZeroSessionId);
    }
    let mut out = [0u8; TICKET_GRANT_BYTES];
    out[..4].copy_from_slice(&TICKET_GRANT_MAGIC);
    out[4] = role as u8;
    out[5..21].copy_from_slice(&session_id.as_raw().to_be_bytes());
    out[21..53].copy_from_slice(ticket);
    Ok(out)
}

/// Parse a server-issued `FRTK` auxiliary ticket grant frame.
pub fn parse_ticket_grant(
    bytes: &[u8],
) -> Result<(RemoteSessionId, BrowserChannelKind, [u8; 32]), BrowserProtocolError> {
    if bytes.len() != TICKET_GRANT_BYTES {
        return Err(BrowserProtocolError::InvalidLength);
    }
    if bytes[..4] != TICKET_GRANT_MAGIC {
        return Err(BrowserProtocolError::MalformedMagic);
    }
    let role = BrowserChannelKind::from_byte(bytes[4])?;
    let mut sid_bytes = [0u8; 16];
    sid_bytes.copy_from_slice(&bytes[5..21]);
    let session_id = RemoteSessionId::from_raw(u128::from_be_bytes(sid_bytes));
    if session_id.as_raw() == 0 {
        return Err(BrowserProtocolError::ZeroSessionId);
    }
    let mut ticket = [0u8; 32];
    ticket.copy_from_slice(&bytes[21..53]);
    Ok((session_id, role, ticket))
}
