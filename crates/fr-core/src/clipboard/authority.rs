//! Read-only authority for a clipboard lane, independent of native input APIs.
//!
//! The host delegates to its original input monitor. A viewer supplies a monitor
//! of its already accepted control grant and qualified host-clock projection;
//! it never constructs an `InputSession` or grants host authority locally.
use super::Binding;
use crate::{
    ids::InputTicketId,
    input_submission::{InputMonitor, InputSession, Refusal},
    time::HostInstant,
};
use core::fmt;
use std::sync::Arc;

/// Trusted local lifecycle integration, NOT a peer-supplied capability. Checks
/// must be bounded, nonblocking, and tied to the original owner, not just equal
/// numeric IDs. A failed/expired owner must never become usable again. The
/// viewer implementation must include its clock uncertainty and local lifecycle.
/// No native work, transport operation, or caller callback may run under a lock.
/// This interface cannot grant input, issue tickets, or renew a host lease.
pub trait Authority: Send + Sync {
    fn binding(&self) -> Binding;
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal>;
    fn revoke(&self);
}

/// Shared read/revoke-only access. Cloning retains the SAME owner; it is not a
/// new grant or a way to reset a clipboard channel's consumed sequence floors.
#[derive(Clone)]
pub struct Monitor(Arc<dyn Authority>);
impl fmt::Debug for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardMonitor([original owner])")
    }
}
impl Monitor {
    pub fn new(source: impl Authority + 'static) -> Self {
        Self(Arc::new(source))
    }
    /// Delegates to the real host owner, preserving all existing expiry and
    /// revocation behavior. The zero ticket is an immutable scope query only.
    pub fn from_input(input: &InputSession) -> Self {
        let scope = input.ticket_credentials(InputTicketId::from_raw(0));
        Self::new(Host {
            input: input.monitor(),
            binding: Binding {
                session: scope.session,
                lease: scope.lease,
            },
        })
    }
    pub fn binding(&self) -> Binding {
        self.0.binding()
    }
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.0.deadline(now)
    }
    pub fn revoke(&self) {
        self.0.revoke();
    }
}
struct Host {
    input: InputMonitor,
    binding: Binding,
}
impl Authority for Host {
    fn binding(&self) -> Binding {
        self.binding
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.input.deadline(now)
    }
    fn revoke(&self) {
        self.input.revoke();
    }
}
