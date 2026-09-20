//! Pending decoders occupy the SAME bounded subscriber cohort and send caches.
//! No native operation, permission renewal or caller callback runs under its lock.
use super::{
    CaptureSource, Entry, Error, MediaEpoch, MediaError, ObservationControl, Publisher, SendReport,
    Subscriber, Subscription, decoder_startup, same_source_view, same_task,
};
use crate::media_quic::{NegotiatedMedia, QuicEgress};
use fr_transport::quic::QuicRecords;
use std::sync::Arc;

pub(super) struct Starting {
    pub(super) host: decoder_startup::Host,
    pub(super) first: u64,
    pub(super) seeded: bool,
}
impl Publisher {
    /// Admit one already-authorized observation viewer BEFORE its decoder is
    /// configured. `startup` must be an unstarted shared host handshake for the
    /// actual current source IDR and this exact physical pool. `sender` must be
    /// this viewer's original, empty egress on completed media attachments.
    ///
    /// Pending and complete viewers share the eight-slot bound. Configuration,
    /// original IDR and first-decode acknowledgements are driven by Subscriber's
    /// ordinary bounded service on the original connection. A pending viewer
    /// retains its original capture-anchored startup deadline and logical budget;
    /// neither capture nor another viewer's progress extends them.
    ///
    /// This does not authorize a viewer, select a new source, force a late-join
    /// IDR, or grant input. Existing connections must still drive their own UDP,
    /// observation renewal and cancellation. Admission needs exclusive source
    /// ownership; it cannot race a native capture in flight.
    pub fn admit_pending(
        &mut self,
        mut startup: decoder_startup::Host,
        sender: QuicEgress,
        media: NegotiatedMedia,
        transport: &QuicRecords,
    ) -> Result<Subscriber, Error> {
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let slot = members
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        let (control, view) = startup
            .pending_publication(transport, &self.source, &self.pool)
            .map_err(Error::Startup)?;
        if control.same_owner(&members.owner)
            || members.owner.belongs_to_session(view.parent.remote_session)
            || same_task(&control, &members.owner)
            || members.entries.iter().flatten().any(|e| {
                e.control.same_owner(&control)
                    || same_task(&e.control, &control)
                    || e.media.binding().parent.remote_session == view.parent.remote_session
            })
            || members.anchor.is_some_and(|a| !same_source_view(a, view))
        {
            return Err(Error::WrongSource);
        }
        sender
            .join_pending_publisher(
                transport,
                &media,
                &self.source,
                &members.owner,
                &control,
                view,
            )
            .map_err(Error::Transport)?;
        let first = self.source.last_capture.ok_or(Error::WrongSource)?.as_raw();
        members.anchor.get_or_insert(view);
        members.started = true;
        members.entries[slot] = Some(Entry {
            connection: transport.binding(),
            media,
            control,
            sender,
            failure: None,
            join: None,
            starting: Some(Starting {
                host: startup,
                first,
                seeded: false,
            }),
        });
        Ok(Subscriber {
            members: Arc::downgrade(&self.members),
            slot,
        })
    }
}
impl Entry {
    /// One bounded receive pass and at most one configuration record. Native
    /// decoder work remains on the peer. Exact configuration retries and the
    /// `FirstDecoded` gate remain in the original host state machine.
    pub(super) fn service_startup(
        &mut self,
        transport: &mut QuicRecords,
        owner: &ObservationControl,
        report: &mut SendReport,
    ) -> Result<(), Error> {
        let Some(starting) = &mut self.starting else {
            return Ok(());
        };
        starting.host.dispatch(transport).map_err(Error::Startup)?;
        if starting.host.configuration_pending() {
            if starting
                .host
                .transmit_authorized(transport, || owner.check().is_ok())
                .map_err(Error::Startup)?
            {
                report.accepted += 1;
            } else {
                report.pending = true;
            }
        }
        if let Some(update) = starting
            .host
            .take_shared_recovery()
            .map_err(Error::Startup)?
        {
            if starting.seeded || update.frame().as_raw() != starting.first {
                return Err(Error::WrongSource);
            }
            self.sender
                .enqueue_shared_capture(&update)
                .map_err(Error::Transport)?;
            starting.seeded = true;
        }
        if starting.host.is_complete() {
            // No later source frame, fabricated decode or copied authority may
            // stand in for this connection's exact original acknowledgement.
            let starting = self.starting.take().ok_or(Error::Closed)?;
            let (control, view) = starting
                .host
                .finish_stream(transport)
                .map_err(Error::Startup)?;
            if !control.same_owner(&self.control) || view != self.media.binding() {
                return Err(Error::WrongSource);
            }
        }
        Ok(())
    }
}
impl Subscriber {
    /// Whether this viewer's exact first-decode report completed the host gate.
    /// This is peer-reported decoding, not physical visibility or an input grant.
    pub fn startup_complete(&mut self, transport: &QuicRecords) -> Result<bool, Error> {
        self.is_ready(transport)
    }
}
impl Subscription {
    pub(crate) fn join_pending_source(
        &self,
        source: &CaptureSource,
        owner: &ObservationControl,
        control: &ObservationControl,
        epoch: MediaEpoch,
    ) -> Result<(), MediaError> {
        owner.check()?;
        control.check()?;
        if !self.control.same_owner(control)
            || owner.same_owner(control)
            || self.epoch != epoch
            || !self.first
            || self.capture_source.is_some()
            || self.source_progress().is_some()
            || source.configuration.generation != epoch.configuration
            || source
                .selected_control
                .as_ref()
                .is_some_and(|c| !c.same_owner(owner))
        {
            return Err(MediaError::InvalidFrame);
        }
        Ok(())
    }
}
