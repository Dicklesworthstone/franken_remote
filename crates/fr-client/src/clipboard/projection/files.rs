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
/// Cloning retains only a weak lifetime and the same clock consumers; it cannot
/// keep the controller alive, renew a grant or reopen its one-use Files lane.
#[derive(Clone)]
pub struct ControllerAuthority {
    state: Weak<State>,
    cursor: Arc<Mutex<Cursor>>,
    checks: Arc<Mutex<Cursor>>,
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
    /// Check ongoing file work without producing a native publication sample.
    /// Source scans and their supervising session can run concurrently: their
    /// trusted local samples may arrive out of order. Keep their conservative
    /// maximum on a separate cursor, so neither reader can invalidate the disk
    /// owner's exact publication sample or misclassify another reader as a
    /// regressed clock. This never renews the original grant or its deadline.
    pub fn check(&self, now: ClientInstant) -> Result<(), Refusal> {
        let state = self.state.upgrade().ok_or(Refusal::Revoked)?;
        let mut cursor = self
            .checks
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let mut metadata = state
            .inner
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let (host, local) = state
            .sample_cursor_locked(now, &mut cursor, &mut metadata, false, true)
            .map_err(|_| {
                state.stopped.store(true, Ordering::Release);
                Refusal::Revoked
            })?;
        if local.0 >= metadata.local_until || host.as_micros() >= metadata.host_until {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        Ok(())
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
            checks: Arc::new(Mutex::new(Cursor {
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
    fn concurrent_source_checks_preserve_the_original_publication_sample() {
        let mut owner = owner();
        let authority = owner.attach_files(5, ClientInstant(100)).unwrap();
        let publication = authority.sample(ClientInstant(101)).unwrap();
        let source = authority.clone();

        source.check(ClientInstant(300)).unwrap();
        authority.check(ClientInstant(200)).unwrap();
        source.check(ClientInstant(299)).unwrap();
        assert_eq!(
            authority.deadline(publication),
            Ok(HostInstant::from_micros(1_000_000))
        );
        assert!(!authority.is_revoked());

        // The separate readiness cursor cannot relax the disk owner's ordered
        // clock, including its final nonblocking publication sample.
        authority.try_sample(ClientInstant(301)).unwrap();
        assert!(authority.sample(ClientInstant(300)).is_err());
        assert!(source.check(ClientInstant(302)).is_err());
        owner.check_files(ClientInstant(302)).unwrap_err();
    }

    #[test]
    fn concurrent_source_readers_do_not_revoke_each_other() {
        let mut owner = owner();
        let authority = owner.attach_files(5, ClientInstant(100)).unwrap();
        let next = std::sync::atomic::AtomicU64::new(101);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let reader = authority.clone();
                let next = &next;
                scope.spawn(move || {
                    for _ in 0..256 {
                        // Samples are monotonic at capture, but the threads
                        // can enter the shared check in a different order.
                        let now = next.fetch_add(1, Ordering::Relaxed);
                        std::thread::yield_now();
                        reader.check(ClientInstant(now)).unwrap();
                    }
                });
            }
        });
        assert!(!authority.is_revoked());
        let publication = authority.try_sample(ClientInstant(2_000)).unwrap();
        authority.deadline(publication).unwrap();
    }

    #[test]
    fn source_checks_keep_expiry_revocation_and_original_owner_terminal() {
        for failure in 0..3 {
            let mut owner = owner();
            let authority = owner.attach_files(5, ClientInstant(100)).unwrap();
            authority.check(ClientInstant(999_999)).unwrap();
            match failure {
                0 => assert!(authority.check(ClientInstant(1_000_000)).is_err()),
                1 => authority.revoke(),
                _ => drop(owner),
            }
            // An overtaken clock is allowed only while the SAME grant lives.
            assert!(authority.check(ClientInstant(100)).is_err());
            assert!(authority.sample(ClientInstant(1_000_001)).is_err());
            assert!(authority.is_revoked());
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
