//! Local source renewal is not a viewer heartbeat, capture receipt or input grant.
use super::{Error as PublishError, Members, Publisher, same_source_view};
use crate::{media::Error as MediaError, session_agent::SessionAgent};
use fr_core::time::HostInstant;
use fr_wire::decoder::Binding;
use std::sync::{Arc, Mutex, Weak, atomic::Ordering};

const CADENCE_US: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Closed,
    Full,
    AlreadyAttached,
    NotIndependent,
    NoCapturePermission,
    SessionLocked,
    SessionChanged,
    WrongScope,
    NonceUnavailable,
    Poisoned,
    Media(MediaError),
    Publisher(PublishError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Permission deadlines only. Neither timestamp says that pixels are fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub renewed: bool,
    pub authorized_until: HostInstant,
    pub next_check: HostInstant,
}
struct Cadence {
    next: u64,
    last_nonce: Option<u128>,
}
/// Only the original local agent owns a registration. Copies used during a
/// bounded maintenance pass contain no source, frame, worker or transport owner.
pub(crate) struct Renewal {
    members: Weak<Mutex<Members>>,
    scope: Binding,
    os_session: u32,
    cadence: Mutex<Cadence>,
}
impl Renewal {
    pub(crate) fn attach(publisher: &Publisher, agent: &SessionAgent) -> Result<Self, Error> {
        let os_session = agent.permissions().os_session_id();
        Self::permission(agent, os_session)?;
        let mut members = publisher.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick().map_err(Error::Publisher)?;
        let scope = members.anchor.ok_or(Error::WrongScope)?;
        if members.active() == 0 {
            return Err(Error::Closed);
        }
        // A peer-bound or control-grant owner is not an independent OS source.
        // The existing sticky bit also prevents a network renewer attaching later.
        if members.owner.admission.is_some()
            || members.owner.control_grant_attached.load(Ordering::Acquire)
        {
            return Err(Error::NotIndependent);
        }
        members
            .owner
            .renewal_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::AlreadyAttached)?;
        Ok(Self {
            members: Arc::downgrade(&publisher.members),
            scope,
            os_session,
            cadence: Mutex::new(Cadence {
                next: members.last,
                last_nonce: None,
            }),
        })
    }
    fn permission(agent: &SessionAgent, os_session: u32) -> Result<(), Error> {
        if agent.is_revoked() {
            return Err(Error::Closed);
        }
        let permissions = agent.permissions();
        if permissions.os_session_id() != os_session {
            return Err(Error::SessionChanged);
        }
        if permissions.is_locked() {
            return Err(Error::SessionLocked);
        }
        permissions
            .verify_screen_capture()
            .map_err(|_| Error::NoCapturePermission)
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.members
            .upgrade()
            .is_none_or(|m| m.lock().map_or(true, |m| m.closed))
    }
    pub(crate) fn close(&self) {
        if let Some(members) = self.members.upgrade() {
            members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close(PublishError::Closed);
        }
    }
    pub(crate) fn service(
        &self,
        agent: &SessionAgent,
        fresh_nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) -> Result<Status, Error> {
        // Protect the source even when a local adapter unwinds through this
        // maintenance call. An event loop catching a panic must not accidentally
        // retain the old observation grant until its ordinary expiry.
        let mut operation = RenewalOperation {
            owner: self,
            finished: false,
        };
        let result = self.service_inner(agent, fresh_nonce);
        operation.finished = result.is_ok();
        result
    }
    fn service_inner(
        &self,
        agent: &SessionAgent,
        fresh_nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) -> Result<Status, Error> {
        Self::permission(agent, self.os_session)?;
        let members = self.members.upgrade().ok_or(Error::Closed)?;
        let (control, now, until) = {
            let mut members = members.lock().map_err(|_| Error::Poisoned)?;
            members.tick().map_err(Error::Publisher)?;
            if members
                .anchor
                .is_none_or(|a| !same_source_view(a, self.scope))
            {
                return Err(Error::WrongScope);
            }
            let now = members.owner.check().map_err(Error::Media)?;
            let until = members
                .owner
                .authority
                .lock()
                .map_err(|_| Error::Poisoned)?
                .observation_deadline(now)
                .map_err(|e| Error::Media(MediaError::Authority(e)))?;
            (members.owner.clone(), now, until)
        };
        let (next, last_nonce) = {
            let cadence = self.cadence.lock().map_err(|_| Error::Poisoned)?;
            (cadence.next, cadence.last_nonce)
        };
        if now.as_micros() < next {
            return Ok(Status {
                renewed: false,
                authorized_until: until,
                next_check: HostInstant::from_micros(next.min(until.as_micros())),
            });
        }
        // No source/registry/authority mutex surrounds caller entropy generation.
        // A concurrent local revoke can fence all members while it is running.
        let nonce = fresh_nonce().map_err(|()| Error::NonceUnavailable)?;
        if nonce == 0 || last_nonce == Some(nonce) {
            return Err(Error::NonceUnavailable);
        }
        Self::permission(agent, self.os_session)?;
        let mut members = members.lock().map_err(|_| Error::Poisoned)?;
        members.tick().map_err(Error::Publisher)?;
        let issued = control.check().map_err(Error::Media)?;
        let fixed = control.issue_challenge(nonce).map_err(Error::Media)?;
        // This answer is a fresh LOCAL consent check on the same authority, never
        // a remote record. Delayed service cannot resurrect expired observation.
        Self::permission(agent, self.os_session)?;
        let until = control.renew(nonce).map_err(Error::Media)?;
        debug_assert_eq!(fixed, until);
        let remaining = until.as_micros().saturating_sub(issued.as_micros());
        let next = issued
            .as_micros()
            .checked_add(CADENCE_US.min((remaining / 3).max(1)))
            .ok_or(Error::Closed)?;
        let mut cadence = self.cadence.lock().map_err(|_| Error::Poisoned)?;
        cadence.last_nonce = Some(nonce);
        cadence.next = next;
        Ok(Status {
            renewed: true,
            authorized_until: until,
            next_check: HostInstant::from_micros(next),
        })
    }
}
impl Drop for Renewal {
    fn drop(&mut self) {
        self.close();
    }
}

struct RenewalOperation<'a> {
    owner: &'a Renewal,
    finished: bool,
}
impl Drop for RenewalOperation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.close();
        }
    }
}
