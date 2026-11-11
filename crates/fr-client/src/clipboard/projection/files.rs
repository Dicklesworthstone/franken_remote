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
    /// Lock-free lifetime fence for a publication whose policy was just checked.
    /// This does not replace the clock, readiness or lease checks.
    pub fn is_revoked(&self) -> bool {
        self.state
            .upgrade()
            .is_none_or(|state| state.stopped.load(Ordering::Acquire))
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
    /// Project a final native clock without waiting for either policy lock.
    /// Used after blocking publication authorization: a busy owner refuses the
    /// attempt instead of spending its already sampled lifetime in a lock wait.
    /// This updates the SAME consumer cursor and never renews its lease proof.
    pub fn try_sample(&self, now: ClientInstant) -> Result<HostInstant, Refusal> {
        let state = self.state.upgrade().ok_or(Refusal::Revoked)?;
        let mut cursor = self
            .cursor
            .try_lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let mut metadata = state
            .inner
            .try_lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let (host, local) = state
            .sample_cursor_locked(now, &mut cursor, &mut metadata, false, false)
            .map_err(|_| {
                state.stopped.store(true, Ordering::Release);
                Refusal::Revoked
            })?;
        if local.0 >= metadata.local_until || host.as_micros() >= metadata.host_until {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        Ok(host)
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
                sampled_client: now.0,
                client: now.0,
                host: m.last_host,
            })),
            parent: self.state.parent,
            lease: self.state.binding.lease,
            channel,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::projection::Metadata;
    use fr_core::{
        clipboard::Binding,
        ids::{HostBootId, OsSessionId},
    };
    use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
    use std::sync::atomic::AtomicBool;

    fn owner() -> Owner {
        let parent = ControlBinding {
            id: 1,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        };
        Owner {
            attached: false,
            state: Arc::new(State {
                stopped: AtomicBool::new(false),
                binding: Binding {
                    session: parent.remote_session,
                    lease: InputLeaseId::from_raw(4),
                },
                parent,
                display_channel: 2,
                input_channel: 3,
                inner: Mutex::new(Metadata {
                    receiver: None,
                    clock: ClockCorrelation::new(
                        ClockSample {
                            host_boot: parent.host_boot,
                            client_sent_us: 0,
                            client_received_us: 100,
                            host_sample_us: 0,
                        },
                        ClockPolicy {
                            drift_ppm: 0,
                            ..ClockPolicy::default()
                        },
                    )
                    .unwrap(),
                    last_client: 100,
                    last_host: 100,
                    host_until: 1_000_000,
                    local_until: 1_000_000,
                    mapped: true,
                    view_until: Some(10_000),
                    obligations_until: 1_000_000,
                    last_view_until: Some(10_000),
                    paused_since: None,
                    resume_ticket: false,
                    reference_recovery: false,
                }),
            }),
        }
    }

    #[test]
    fn final_projection_never_waits_on_either_original_policy_mutex() {
        for cursor in [true, false] {
            let mut owner = owner();
            let authority = owner.attach_files(5, ClientInstant(100)).unwrap();
            let cursor_guard = cursor.then(|| authority.cursor.lock().unwrap());
            let state_guard = (!cursor).then(|| owner.state.inner.lock().unwrap());
            let other = authority.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                tx.send(other.try_sample(ClientInstant(101))).unwrap();
            });
            let result = rx.recv_timeout(std::time::Duration::from_secs(5));
            // Release before asserting: a regressed blocking implementation
            // cannot leave a hung test thread behind after the failure.
            drop(cursor_guard);
            drop(state_guard);
            worker.join().unwrap();
            assert_eq!(result.unwrap(), Err(Refusal::AuthorityUnavailable));
            assert!(!authority.is_revoked());
            // Refusing one boundary creates no new projection or owner epoch.
            authority.try_sample(ClientInstant(102)).unwrap();
        }
    }
}
