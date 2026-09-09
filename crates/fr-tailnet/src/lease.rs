//! An independently checked admission lifetime shared with media/input owners.
//! Refresh does no work while holding the gate lock. A dropped owner or refresh
//! is terminal; an old grant is never revived by a late successful `LocalAPI` read.
use crate::{Authorization, ConnectionAddresses, Error, LocalApi, Permissions};
use asupersync::cx::Cx;
use std::{
    fmt,
    sync::{Arc, Mutex},
};

struct State {
    proof: Arc<Authorization>,
    stopped: Option<Error>,
    last_us: u64,
}

/// Shared checks, not independent authority copies. Check this immediately at
/// each effect/packet admission. Cloning a lease cannot extend its deadline.
#[derive(Clone)]
pub struct Lease {
    state: Arc<Mutex<State>>,
    api: LocalApi,
    cx: Cx,
    addresses: ConnectionAddresses,
}
impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Lease([shared admission gate])")
    }
}
impl Lease {
    fn live(&self, s: &mut State) -> Result<Permissions, Error> {
        if let Some(error) = s.stopped {
            return Err(error);
        }
        let result = (|| {
            let now = crate::local::now(&self.cx)?;
            if now < s.last_us {
                return Err(Error::Clock);
            }
            s.last_us = now;
            self.api.check(&self.cx, &s.proof, self.addresses)
        })();
        if let Err(error) = result {
            s.stopped = Some(error);
        }
        result
    }
    pub fn check(&self) -> Result<Permissions, Error> {
        let mut s = self.state.lock().map_err(|_| Error::Revoked)?;
        self.live(&mut s)
    }
    pub fn observe(&self) -> Result<u64, Error> {
        let mut s = self.state.lock().map_err(|_| Error::Revoked)?;
        if !self.live(&mut s)?.observe() {
            return Err(Error::CapabilityDenied);
        }
        Ok(s.proof.expires_us())
    }
    /// Permission only: a control lease, view, ticket and final native gate are
    /// still required. Read-only admission must never acquire those effects.
    pub fn control(&self) -> Result<u64, Error> {
        let mut s = self.state.lock().map_err(|_| Error::Revoked)?;
        if !self.live(&mut s)?.control() {
            return Err(Error::CapabilityDenied);
        }
        Ok(s.proof.expires_us())
    }
    pub fn addresses(&self) -> ConnectionAddresses {
        self.addresses
    }
    /// Local lifecycle stop. Never waits for `LocalAPI`, a media worker or input.
    pub fn revoke(&self) {
        self.stop(Error::Revoked);
    }
    fn stop(&self, error: Error) {
        if let Ok(mut s) = self.state.lock() {
            s.stopped.get_or_insert(error);
        }
    }
}

/// One refresh owner for an admitted transport peer. No background task is
/// created: the containing structured session schedules refresh and teardown.
pub struct Admission {
    lease: Lease,
}
impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Admission([owned lifetime])")
    }
}
impl Admission {
    pub fn new(api: LocalApi, cx: Cx, proof: Authorization) -> Result<Self, Error> {
        let addresses = proof.addresses();
        api.check(&cx, &proof, addresses)?;
        let last_us = crate::local::now(&cx)?;
        let owner = Self {
            lease: Lease {
                state: Arc::new(Mutex::new(State {
                    proof: Arc::new(proof),
                    stopped: None,
                    last_us,
                })),
                api,
                cx,
                addresses,
            },
        };
        owner.lease.check()?;
        Ok(owner)
    }
    /// Retained authority context. Session deadlines use this exact clock;
    /// do not compare admission timestamps with another runtime clock.
    pub fn context(&self) -> Cx {
        self.lease.cx.clone()
    }
    pub fn lease(&self) -> Lease {
        self.lease.clone()
    }
    pub fn revoke(&self) {
        self.lease.revoke();
    }
    /// Revalidate before the existing deadline. Any failure stops this admission.
    /// Permission changes also end it: neither privilege escalation nor downgrade
    /// is silently applied to an already granted controller/observation session.
    pub async fn refresh(&mut self) -> Result<(), Error> {
        let old = {
            let mut s = self.lease.state.lock().map_err(|_| Error::Revoked)?;
            self.lease.live(&mut s)?;
            s.proof.clone()
        };
        let mut guard = Refresh {
            lease: &self.lease,
            armed: true,
        };
        let result = self
            .lease
            .api
            .revalidate(&self.lease.cx, &old, self.lease.addresses)
            .await;
        let proof = match result {
            Ok(proof) => proof,
            Err(error) => {
                self.lease.stop(error);
                return Err(error);
            }
        };
        {
            let mut s = self.lease.state.lock().map_err(|_| Error::Revoked)?;
            self.lease.live(&mut s)?;
            if !Arc::ptr_eq(&s.proof, &old) || old.permissions() != proof.permissions() {
                s.stopped = Some(Error::CapabilityDenied);
                return Err(Error::CapabilityDenied);
            }
            s.proof = Arc::new(proof);
            self.lease.live(&mut s)?;
        }
        guard.armed = false;
        Ok(())
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.lease.revoke();
    }
}
struct Refresh<'a> {
    lease: &'a Lease,
    armed: bool,
}
impl Drop for Refresh<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.lease.revoke();
        }
    }
}
