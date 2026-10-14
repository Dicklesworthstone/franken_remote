//! The typed reason in the host's terminal close-packet fallback
//! (`PROTOCOL_TRANSPORT_CLOSE.md`): one application error code per existing
//! Closed or `LeaseRevoked` reason. It names why the peer closed; it is NOT a
//! Closed or `LeaseRevoked` report and carries no session, lease, cleanup or
//! effect information.
use asupersync::net::quic_native::QuicConnection;
use fr_wire::{closure::ClosedReason, lease_revoked::Reason};

const CLOSED_REASON_BASE: u64 = 0x4652_0100;
const REVOKED_REASON_BASE: u64 = 0x4652_0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClose {
    /// A session-scope Closed reason.
    Session(ClosedReason),
    /// A control-lease `LeaseRevoked` reason.
    Lease(Reason),
}
impl PeerClose {
    /// The application error code for this reason.
    #[must_use]
    pub const fn code(self) -> u64 {
        match self {
            Self::Session(reason) => CLOSED_REASON_BASE + reason as u64,
            Self::Lease(reason) => REVOKED_REASON_BASE + reason as u64,
        }
    }
    /// Only an assigned reason in either family; zero, unassigned reasons and
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
        reason(REVOKED_REASON_BASE)
            .and_then(Reason::from_wire)
            .map(Self::Lease)
    }
    /// The peer's own close on this established connection, if it carried a
    /// recognized reason. A local close or idle expiry is never attributed.
    pub(super) fn of(connection: &QuicConnection) -> Option<Self> {
        if !connection.close_was_peer_initiated() {
            return None;
        }
        connection
            .inner()
            .transport()
            .close_code()
            .and_then(Self::from_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_assigned_reason_round_trips_and_nothing_else_decodes() {
        let mut decoded = 0;
        for code in (0..0x300_u64).flat_map(|low| [0x4652_0000 + low, low]) {
            if let Some(peer) = PeerClose::from_code(code) {
                assert_eq!(peer.code(), code);
                decoded += 1;
            }
        }
        // ClosedReason 1-8 and LeaseRevoked Reason 1-9.
        assert_eq!(decoded, 8 + 9);
        assert_eq!(
            PeerClose::from_code(0x4652_0107),
            Some(PeerClose::Session(ClosedReason::HostFailure))
        );
        assert_eq!(
            PeerClose::from_code(0x4652_0201),
            Some(PeerClose::Lease(Reason::LocalRevoke))
        );
        for unassigned in [
            0x4652_0100,
            0x4652_0109,
            0x4652_0200,
            0x4652_020a,
            0,
            u64::MAX,
        ] {
            assert_eq!(PeerClose::from_code(unassigned), None);
        }
    }
}
