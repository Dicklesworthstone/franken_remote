//! The typed reason in the host's terminal close-packet fallback
//! (`PROTOCOL_TRANSPORT_CLOSE.md`): one application error code per existing
//! Closed or `LeaseRevoked` reason. It names why the peer closed; it is NOT a
//! Closed or `LeaseRevoked` report and carries no session, lease, cleanup or
//! effect information.
use asupersync::net::quic_native::QuicConnection;
use fr_wire::{closure::ClosedReason, lease_revoked::Reason};

const CLOSED_REASON_BASE: u64 = 0x4652_0100;
const REVOKED_REASON_BASE: u64 = 0x4652_0200;
const CLIENT_REASON_BASE: u64 = 0x4652_0300;

/// Why the original client stopped ordinary I/O. This is terminal metadata,
/// never a claim that the host released input or completed an application action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ClientCloseReason {
    Failure = 1,
    DeadlineExpired = 2,
    AuthorityLost = 3,
    Cancelled = 4,
}
impl ClientCloseReason {
    pub(super) const fn from_error(error: super::Error) -> Self {
        match error {
            super::Error::Expired
            | super::Error::SendExpired
            | super::Error::Stream(fr_wire::stream::StreamError::Expired) => Self::DeadlineExpired,
            super::Error::Unauthorized => Self::AuthorityLost,
            super::Error::Cancelled => Self::Cancelled,
            _ => Self::Failure,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClose {
    /// A session-scope Closed reason.
    Session(ClosedReason),
    /// A control-lease `LeaseRevoked` reason.
    Lease(Reason),
    /// An abrupt client end, scoped to the original authenticated connection.
    Client(ClientCloseReason),
}
impl PeerClose {
    /// The application error code for this reason.
    #[must_use]
    pub const fn code(self) -> u64 {
        match self {
            Self::Session(reason) => CLOSED_REASON_BASE + reason as u64,
            Self::Lease(reason) => REVOKED_REASON_BASE + reason as u64,
            Self::Client(reason) => CLIENT_REASON_BASE + reason as u64,
        }
    }
    /// Only an assigned reason in these families; zero, unassigned reasons and
    /// every other code (including all QUIC transport error codes) are None.
    #[must_use]
    pub fn from_code(code: u64) -> Option<Self> {
        let reason = |base: u64| {
            code.checked_sub(base)
                .filter(|value| *value < 0x100)
                .and_then(|value| u16::try_from(value).ok())
        };
        if let Some(value) = reason(CLOSED_REASON_BASE) {
            return ClosedReason::from_wire(value).map(Self::Session);
        }
        if let Some(value) = reason(REVOKED_REASON_BASE) {
            return Reason::from_wire(value).map(Self::Lease);
        }
        match reason(CLIENT_REASON_BASE)? {
            1 => Some(Self::Client(ClientCloseReason::Failure)),
            2 => Some(Self::Client(ClientCloseReason::DeadlineExpired)),
            3 => Some(Self::Client(ClientCloseReason::AuthorityLost)),
            4 => Some(Self::Client(ClientCloseReason::Cancelled)),
            _ => None,
        }
    }
    /// The peer's own close on this established connection, if it carried a
    /// recognized reason. A local close or idle expiry is never attributed.
    pub(super) fn of(connection: &QuicConnection) -> Option<Self> {
        if !connection.close_was_peer_initiated() {
            return None;
        }
        let reason = connection
            .inner()
            .transport()
            .close_code()
            .and_then(Self::from_code)?;
        // A host cannot attribute a client failure to itself. Direction is
        // checked on the authenticated native owner, never on peer metadata.
        if matches!(reason, Self::Client(_))
            && connection.role() != asupersync::net::quic_native::StreamRole::Server
        {
            return None;
        }
        Some(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_assigned_reason_round_trips_and_nothing_else_decodes() {
        let mut decoded = 0;
        for code in (0..0x400_u64).flat_map(|low| [0x4652_0000 + low, low]) {
            if let Some(peer) = PeerClose::from_code(code) {
                assert_eq!(peer.code(), code);
                decoded += 1;
            }
        }
        // ClosedReason 1-9, LeaseRevoked Reason 1-10, ClientCloseReason 1-4.
        assert_eq!(decoded, 9 + 10 + 4);
        assert_eq!(
            PeerClose::from_code(0x4652_0107),
            Some(PeerClose::Session(ClosedReason::HostFailure))
        );
        assert_eq!(
            PeerClose::from_code(0x4652_0201),
            Some(PeerClose::Lease(Reason::LocalRevoke))
        );
        assert_eq!(
            PeerClose::from_code(0x4652_0109),
            Some(PeerClose::Session(ClosedReason::GeometryChanged))
        );
        assert_eq!(
            PeerClose::from_code(0x4652_020a),
            Some(PeerClose::Lease(Reason::GeometryChanged))
        );
        for unassigned in [
            0x4652_0100,
            0x4652_010a,
            0x4652_0200,
            0x4652_020b,
            0x4652_0300,
            0x4652_0305,
            0,
            u64::MAX,
        ] {
            assert_eq!(PeerClose::from_code(unassigned), None);
        }
    }
}
