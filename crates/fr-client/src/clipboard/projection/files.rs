//! File consumers use a separate original-owner projection. Unlike clipboard,
//! a bounded stale view permits file receipt, but an expired lease proof never
//! permits disk work or publication, even while input is suspended.
use super::{Cursor, Error, Owner, State};
use crate::input::ClientInstant;
use fr_core::{
    authority::AuthorityError,
    ids::{InputLeaseId, RemoteSessionId},
    input_submission::Refusal,
    time::HostInstant,
};
use fr_wire::negotiation::ControlBinding;
use std::sync::{Arc, Mutex, Weak, atomic::Ordering};

/// Read-only, revocation-aware handoff from the original accepted controller.
/// Cloning retains only a weak lifetime and the same clock consumer; it cannot
/// keep the controller alive, renew a grant or reopen its one-use Files lane.
#[derive(Clone)]
pub struct ControllerAuthority {
    state: Weak<State>,
    cursor: Arc<Mutex<Cursor>>,
    parent: ControlBinding,
    lease: InputLeaseId,
    channel: u32,
}
impl std::fmt::Debug for ControllerAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControllerFileAuthority([original owner])")
    }
}
impl ControllerAuthority {
    pub const fn parent(&self) -> ControlBinding {
        self.parent
    }
    pub const fn session(&self) -> RemoteSessionId {
        self.parent.remote_session
    }
    pub const fn lease(&self) -> InputLeaseId {
        self.lease
    }
    pub const fn channel(&self) -> u32 {
        self.channel
    }
    /// Revoke files only. This does not revoke input or the clipboard lane.
    pub fn revoke(&self) {
        if let Some(state) = self.state.upgrade() {
            state.stopped.store(true, Ordering::Release);
        }
    }
    /// Sample a trusted CLIENT monotonic clock in the original host's domain.
    /// The disk owner calls this before every external stage, including the
    /// final rename gate. Wire timestamps and newly constructed IDs are not clocks.
    pub fn sample(&self, now: ClientInstant) -> Result<HostInstant, Refusal> {
        let state = self.state.upgrade().ok_or(Refusal::Revoked)?;
        let at = state
            .sample_cursor(now, &self.cursor, false)
            .map_err(|_| Refusal::Revoked)?;
        self.deadline(at)?;
        Ok(at)
    }
    /// Validate this consumer's exact sample. Concurrent input-owner updates may
    /// tighten the decision, never extend an in-flight file's fixed lifetime.
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        let state = self.state.upgrade().ok_or(Refusal::Revoked)?;
        let c = self
            .cursor
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let mut m = state
            .inner
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        if now.as_micros() != c.host {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Revoked);
        }
        let local = c.client.max(m.last_client);
        if state.lifecycle(&mut m, local).is_err() {
            return Err(Refusal::Revoked);
        }
        let host = m
            .clock
            .age_upper_us(0, local)
            .map_err(|_| {
                state.stopped.store(true, Ordering::Release);
                Refusal::Revoked
            })?
            .max(c.host)
            .max(m.last_host);
        if local >= m.local_until || host >= m.host_until {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        Ok(HostInstant::from_micros(m.host_until))
    }
}
impl Owner {
    pub(crate) fn check_files(&self, now: ClientInstant) -> Result<(), Error> {
        let at = self.state.sample(now, false)?;
        let m = self.state.inner.lock().map_err(|_| Error::Stopped)?;
        if now.0 >= m.local_until || at.as_micros() >= m.host_until {
            return self.state.failure(Error::Expired);
        }
        Ok(())
    }
    pub(crate) fn attach_files(
        &mut self,
        channel: u32,
        now: ClientInstant,
    ) -> Result<ControllerAuthority, Error> {
        if self.attached {
            return Err(Error::AlreadyAttached);
        }
        if channel == 0
            || [
                self.state.parent.id,
                self.state.display_channel,
                self.state.input_channel,
            ]
            .contains(&channel)
        {
            return Err(Error::WrongChannel);
        }
        self.check_files(now)?;
        let m = self.state.inner.lock().map_err(|_| Error::Stopped)?;
        if !m.mapped || m.last_view_until.is_none() {
            return Err(Error::NotReady);
        }
        self.attached = true;
        Ok(ControllerAuthority {
            state: Arc::downgrade(&self.state),
            cursor: Arc::new(Mutex::new(Cursor {
                client: now.0,
                host: m.last_host,
            })),
            parent: self.state.parent,
            lease: self.state.binding.lease,
            channel,
        })
    }
}
