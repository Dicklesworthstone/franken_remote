//! Join only the original subscription to its original running session.
use super::{Error, ObservationControl, Subscriber};
use fr_transport::quic::{Disposition, QuicRecords, Route};
use fr_wire::{Progress, decoder::Binding, negotiation::ControlBinding};

impl Subscriber {
    pub(crate) fn original_source(&self) -> super::JoinQueue {
        super::JoinQueue {
            members: self.members.clone(),
        }
    }

    /// Identity preflight is non-mutating, including on a foreign connection or
    /// another authority with equal numeric identifiers. The returned view is
    /// installed media metadata, not a proposed tuple or a readiness grant.
    pub(crate) fn bind_session(
        &mut self,
        q: &QuicRecords,
        control: &ObservationControl,
        parent: ControlBinding,
    ) -> Result<Binding, Error> {
        {
            let shared = self.members.upgrade().ok_or(Error::Closed)?;
            let members = shared.lock().map_err(|_| Error::Poisoned)?;
            let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
            let view = entry.view;
            if !q.is_bound_to(&entry.connection) {
                return Err(Error::ForeignConnection);
            }
            if !entry.control.same_owner(control)
                || view.parent.host_boot != parent.host_boot
                || view.parent.os_session != parent.os_session
                || view.parent.remote_session != parent.remote_session
            {
                return Err(Error::WrongSource);
            }
        }
        self.with_entry(q, |entry| Ok(entry.view))
    }
    /// Check both source consent and this viewer between every native network
    /// wait. A dead source must also fence records already retained by QUIC.
    pub(crate) fn session_live(&self) -> Result<(), Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        entry.failure.map_or(Ok(()), Err)
    }
    /// Only real source progress retained by the canonical sender. An unseeded
    /// join returns None; a heartbeat cannot invent a frame or refresh pixels.
    pub(crate) fn session_progress(&self) -> Result<Option<Progress>, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        if let Some(error) = entry.failure {
            return Err(error);
        }
        if entry.recovery.is_some() {
            return Ok(None);
        }
        entry.sender.source_progress().map_err(Error::Transport)
    }
    pub(crate) fn repair_route(&mut self, q: &QuicRecords) -> Result<Route, Error> {
        self.with_entry(q, |entry| Ok(entry.sender.stream_repair_route()))
    }
    /// Drain only this connection's installed repair lane BEFORE the enclosing
    /// renewal dispatcher. Neither a copied route nor another viewer can select
    /// this sender. No mutex or transport loan crosses an await or user callback.
    pub(crate) fn dispatch_repairs(&mut self, q: &mut QuicRecords) -> Result<usize, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        if !q.is_bound_to(&entry.connection) {
            return Err(Error::ForeignConnection);
        }
        members.tick()?;
        let owner = members.owner.clone();
        let entry = members.entries[self.slot].as_mut().ok_or(Error::Closed)?;
        if let Some(error) = entry.failure {
            return Err(error);
        }
        let control = entry.control.clone();
        let mut failure = None;
        let mut consumed = 0;
        let result = (|| {
            entry.check_media(q)?;
            if entry.recovery.is_some() {
                return Ok(0);
            }
            let expected = entry.sender.stream_repair_route();
            q.receive_ready(
                &control.context(),
                || owner.check().is_ok() && control.check().is_ok(),
                |route| route == expected,
                |route, bytes| {
                    let result = if entry.starting.is_some() || entry.join.is_some() {
                        Err(Error::Startup(super::decoder_startup::Error::WrongState))
                    } else {
                        entry.sender.repair(route, bytes).map_err(Error::Transport)
                    };
                    match result {
                        Ok(_) => {
                            consumed += 1;
                            Ok(Disposition::Consumed)
                        }
                        Err(error) => {
                            failure = Some(error);
                            Err(())
                        }
                    }
                },
            )
            .map_err(|error| {
                failure.unwrap_or(Error::Transport(crate::media_quic::Error::Transport(error)))
            })?;
            Ok(consumed)
        })();
        if let Err(error) = result {
            entry.close(error);
        }
        members.stop_if_empty();
        result
    }
}

impl super::JoinQueue {
    // Numeric boot/display IDs do not authorize joining another capture owner.
    pub(crate) fn service_owner(
        &self,
        publisher: &super::Publisher,
    ) -> Result<ObservationControl, Error> {
        if !std::sync::Weak::ptr_eq(
            &self.members,
            &std::sync::Arc::downgrade(&publisher.members),
        ) {
            return Err(Error::WrongSource);
        }
        let mut members = publisher.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        Ok(members.owner.clone())
    }
}
