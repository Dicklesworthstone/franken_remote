//! Weak lifecycle access to the actual publisher, never copied consent metadata.
use super::{Error, JoinQueue, Members, Publisher};
use fr_wire::decoder::Binding;
use std::sync::{Arc, Mutex, Weak};

pub(crate) struct Publication {
    members: Weak<Mutex<Members>>,
    view: Binding,
}
impl Publication {
    pub(crate) const fn view(&self) -> Binding {
        self.view
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        self.members.ptr_eq(&other.members)
    }
    pub(crate) fn check(&self) -> Result<(), Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        if members.anchor != Some(self.view) {
            return Err(Error::WrongSource);
        }
        Ok(())
    }
    pub(crate) fn claim_registry(&self, owner: &Arc<()>) -> Result<(), Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let identity = Arc::downgrade(owner);
        if members
            .registry_owner
            .as_ref()
            .is_some_and(|current| !current.ptr_eq(&identity))
        {
            return Err(Error::WrongSource);
        }
        members.registry_owner = Some(identity);
        Ok(())
    }
    pub(crate) fn queue(&self) -> Result<JoinQueue, Error> {
        self.check()?;
        Ok(JoinQueue {
            members: self.members.clone(),
        })
    }
    /// Revoke every original authority before dropping retained media. The
    /// publisher's independent task still owns worker cancellation and reaping.
    pub(crate) fn revoke(&self) {
        if let Some(shared) = self.members.upgrade() {
            shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close(Error::Closed);
        }
    }
}
impl Publisher {
    pub(crate) fn publication(&self) -> Result<Publication, Error> {
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let view = members.anchor.ok_or(Error::NoSubscribers)?;
        Ok(Publication {
            members: Arc::downgrade(&self.members),
            view,
        })
    }
}
