//! Renewal-only access to the exact native lease. No native mailbox or OS call.
use super::{
    AuthorityError, HostInstant, InputLeaseId, InputMonitor, InputSession, Refusal,
    RemoteSessionId, SessionAuthority,
};
use std::sync::{Arc, Mutex};

/// One non-cloneable control-renewal capability for an input owner's lifetime.
/// It cannot grant control, change readiness, issue tickets, renew observation,
/// submit input, or certify cleanup. Dropping it fences its native owner.
/// Samples must come from the same host clock as the original grant.
pub struct ControlLease {
    monitor: InputMonitor,
    session: RemoteSessionId,
    lease: InputLeaseId,
    clock: HostInstant,
}
impl InputSession {
    /// Take once; replacing a renewer cannot reuse the old lease's challenge.
    pub fn take_control_lease(&mut self) -> Option<ControlLease> {
        if self.control_taken || self.check_active().is_err() {
            return None;
        }
        self.control_taken = true;
        Some(ControlLease {
            monitor: self.monitor(),
            session: self.session,
            lease: self.lease,
            clock: self.clock,
        })
    }
}
impl ControlLease {
    pub const fn session(&self) -> RemoteSessionId {
        self.session
    }
    pub const fn lease(&self) -> InputLeaseId {
        self.lease
    }
    /// Identity, not structural equality: equal numeric IDs on another approved
    /// owner do not join two independent authority lifetimes.
    pub fn uses_authority(&self, authority: &Arc<Mutex<SessionAuthority>>) -> bool {
        Arc::ptr_eq(&self.monitor.authority, authority)
    }
    pub fn stop(&self) {
        self.monitor.revoke();
    }
    pub fn stopped(&self) -> bool {
        self.monitor.is_revoked()
    }
    pub fn deadline(&mut self, now: HostInstant) -> Result<HostInstant, Refusal> {
        let result = self.monitor.with_time(&mut self.clock, now, |a, at| {
            let until = a.control_deadline()?;
            if at >= until {
                return Err(AuthorityError::LeaseExpired);
            }
            Ok(until)
        });
        if result.is_err() {
            self.stop();
        }
        result
    }
    /// Issue from the serialized pure authority, not a blocked native queue.
    pub fn challenge(&mut self, nonce: u128, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.deadline(now)?;
        self.monitor.with_time(&mut self.clock, now, |a, at| {
            a.issue_control_challenge(nonce, at)
        })
    }
    /// An exact response renews only the still-live original lease, to the
    /// deadline fixed at issue. Neither response time nor a new ticket slides it.
    pub fn respond(&mut self, nonce: u128, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.deadline(now)?;
        self.monitor.with_time(&mut self.clock, now, |a, at| {
            a.respond_control_challenge(self.lease, nonce, at)
        })
    }
}
impl Drop for ControlLease {
    fn drop(&mut self) {
        self.stop();
    }
}
impl std::fmt::Debug for ControlLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlLease")
            .field("stopped", &self.stopped())
            .finish_non_exhaustive()
    }
}
