#![forbid(unsafe_code)]
//! Original host controller proof for an independently approved microphone.
//!
//! This pure Rust handoff can remain in the daemon while `InputSession` moves
//! to its input worker. It performs no native work and grants no audio consent.
//! A native owner or parent process supervisor checks it against the original
//! host clock before admitting work and keeps its own microphone-only fence.

use fr_core::{
    ids::{InputLeaseId, InputTicketId, RemoteSessionId},
    input_submission::{InputMonitor, InputSession, Refusal},
    time::HostInstant,
};

/// Immutable scope used to bind the completed microphone attachment. These
/// public numbers are metadata; constructing them cannot create authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
}

/// Read-only projection of the original host input owner. Only `from_input`
/// constructs it; neither wire credentials nor a caller-supplied predicate can
/// create a projection. Clones retain the same owner and expose no ticket,
/// renewal or local-input-revoke operation. They cannot keep a dropped
/// `InputSession` alive; deadline checks retain its normal terminal expiry fences.
///
/// This is not microphone consent, an acknowledged audio configuration, or a
/// transport attachment. All three require separate checks by their owners.
#[derive(Clone)]
pub struct HostController {
    input: InputMonitor,
    scope: Scope,
}
impl std::fmt::Debug for HostController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostMicrophoneController([original input owner])")
    }
}
impl HostController {
    /// Retain the actual owner before moving it into its worker. The zero
    /// ticket is only the existing immutable scope query and is never issued.
    /// Capturing a projection does not establish that its authority is live;
    /// every admission must still call `deadline` with a fresh original clock.
    pub fn from_input(input: &InputSession) -> Self {
        let scope = input.ticket_credentials(InputTicketId::from_raw(0));
        Self {
            input: input.monitor(),
            scope: Scope {
                session: scope.session,
                lease: scope.lease,
            },
        }
    }

    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// Existing controller authority at a fresh host-monotonic sample. This
    /// retains the input monitor's original lease, observation, view and native
    /// owner checks. A renewal of that SAME owner may extend future admission;
    /// previously admitted packets keep their own immutable receipt deadlines.
    /// A suspended view refuses, and terminal expiry/drop/revoke cannot revive.
    ///
    /// A returned deadline is a current admission cap, never a cached promise
    /// of permission until that time. Recheck before a native effect; explicit
    /// revocation can occur earlier. No callback or foreign call runs here.
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.input.deadline(now)
    }
}
