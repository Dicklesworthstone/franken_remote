//! Recovery retains the admitted subscription instead of minting a new cache.
use super::{Error, NegotiatedMedia, QuicEgress, Routes, same_view};
use crate::media::{CaptureSource, decoder_startup::Setup};
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use fr_media::delivery::RecoveryDemand;
use fr_transport::quic::{ControlRoutes, QuicRecords, Route};
use fr_wire::negotiation::ControlBinding;
use std::time::Duration;

impl NegotiatedMedia {
    /// A recovery startup retains the previous full view and its local absolute
    /// deadline. Positive capability selection and the completed NEXT attachment
    /// set are required; this does not validate native decoder ownership.
    pub fn recovery_decoder_setup(
        &self,
        q: &QuicRecords,
        mut previous: fr_wire::decoder::Binding,
        until_micros: u64,
    ) -> Result<crate::media::decoder_startup::Setup, Error> {
        self.check(q)?;
        self.check_recovery_capability()?;
        previous.recovery = previous.recovery.next().ok_or(Error::InvalidRoutes)?;
        if !same_view(previous, self.binding()) {
            return Err(Error::InvalidRoutes);
        }
        self.decoder_setup(q, std::time::Duration::from_secs(2))
            .map(|setup| setup.capped_at(until_micros))
            .map_err(|_| Error::InvalidRoutes)
    }

    /// Route only the original negotiated control lane to its actual capture
    /// owner. Media-channel numbers and peer-proposed view tuples are not
    /// authority. In a control session the recovery's fence suspends input:
    /// the lease survives unrenewed, and input resumes only with the recovered
    /// generation's fresh evidence and a ticket naming it (plan 11.3, 12.3).
    #[allow(clippy::too_many_arguments)]
    pub fn request_recovery(
        &self,
        q: &QuicRecords,
        control: ControlRoutes,
        parent: ControlBinding,
        sender: &mut QuicEgress,
        source: &mut CaptureSource,
        route: Route,
        bytes: &[u8],
    ) -> Result<bool, Error> {
        let view = self.check_recovery_host(q, control, parent, sender, route)?;
        sender
            .egress
            .request_recovery(source, bytes, view)
            .map_err(Error::Media)
    }

    /// Same original control-route validation as the synchronous request API,
    /// without borrowing capture across network dispatch. Scheduling rechecks the
    /// source when its previous native operation returns.
    pub(crate) fn admit_recovery_request(
        &self,
        q: &QuicRecords,
        control: ControlRoutes,
        parent: ControlBinding,
        sender: &mut QuicEgress,
        bytes: &[u8],
    ) -> Result<Option<RecoveryDemand>, Error> {
        let view =
            self.check_recovery_host(q, control, parent, sender, Route::Stream(control.inbound))?;
        sender
            .egress
            .admit_recovery_request(bytes, view)
            .map_err(Error::Media)
    }

    /// Fence a sender-discovered broken chain and retain its existing deadline
    /// and recovery allowance. Local discovery does not construct a peer record
    /// or issue an encoder demand. The real authenticated request must arrive
    /// before channel replacement and inherits this same absolute deadline.
    pub(crate) fn admit_sender_failure(
        &self,
        q: &QuicRecords,
        control: ControlRoutes,
        parent: ControlBinding,
        sender: &mut QuicEgress,
    ) -> Result<u64, Error> {
        self.check_recovery_host(q, control, parent, sender, Route::Stream(control.inbound))?;
        if !sender
            .egress
            .stream_subscription()
            .map_err(Error::Media)?
            .recovery_pending(self.limits)
        {
            return Err(Error::InvalidRoutes);
        }
        sender.egress.await_sender_recovery().map_err(Error::Media)
    }

    /// Signal an actually failed sender through the existing reliable progress
    /// lane. The last real descriptor/observation stays unchanged; Failed cannot
    /// refresh presentation and fences even a peer which already decoded it.
    /// Only transport admission is reported, not peer receipt or decoder state.
    /// Repeated backpressure attempts encode identical metadata and retain the
    /// original demand deadline. No encoded payload is retained or retried.
    pub(crate) fn notify_sender_failure(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        sender: &QuicEgress,
        mut progress: fr_wire::Progress,
        until: u64,
    ) -> Result<bool, Error> {
        self.check(q)?;
        self.check_recovery_capability()?;
        if !self.is_host()
            || sender.view != Some(self.binding())
            || sender.connection.as_ref().is_none_or(|b| !q.is_bound_to(b))
        {
            return Err(Error::InvalidRoutes);
        }
        let subscription = sender.egress.stream_subscription().map_err(Error::Media)?;
        if !subscription.recovery_pending(self.limits)
            || subscription.recovery_deadline().map_err(Error::Media)? != until
        {
            return Err(Error::InvalidRoutes);
        }
        progress.pipeline = fr_wire::PipelineState::Failed;
        let mut bytes = [0; 128];
        let len = fr_wire::encode_progress(
            progress,
            self.bindings.for_channel(fr_wire::Channel::MediaConfig),
            &self.limits,
            &mut bytes,
        )
        .map_err(|_| Error::InvalidRoutes)?;
        let result = q.send(
            cx,
            Route::Stream(self.video.outbound),
            &bytes[..len],
            until,
            || subscription.recovery_deadline().is_ok_and(|deadline| deadline == until),
        );
        match result {
            Ok(()) => Ok(true),
            Err(fr_transport::quic::Error::Backpressure) => Ok(false),
            Err(error) => Err(Error::Transport(error)),
        }
    }

    pub(crate) fn check_recovery_host(
        &self,
        q: &QuicRecords,
        control: ControlRoutes,
        parent: ControlBinding,
        sender: &QuicEgress,
        route: Route,
    ) -> Result<fr_wire::decoder::Binding, Error> {
        self.check(q)?;
        self.check_recovery_capability()?;
        if q.role().map_err(Error::Transport)? != StreamRole::Server
            || route != Route::Stream(control.inbound)
            || sender.view != Some(self.binding())
            || sender.connection.as_ref().is_none_or(|b| !q.is_bound_to(b))
        {
            return Err(Error::InvalidRoutes);
        }
        super::super::recovery::control_binding(
            q,
            control,
            parent,
            self.binding(),
            *self.limits.protocol(),
        )
        .map_err(|_| Error::InvalidRoutes)
    }

    /// Install the completed NEXT recovery attachment set on the same sender.
    /// All authority/display/configuration fields and media limits must match;
    /// only the recovery generation and retired channel IDs may change. There
    /// is no sender allocation, new recovery allowance, or implicit input grant.
    /// Use the returned setup for native decoder startup: its deadline is capped
    /// by the ORIGINAL failed-chain budget, including time spent on attachments.
    pub fn recover_sender(&self, q: &QuicRecords, sender: &mut QuicEgress) -> Result<Setup, Error> {
        self.check(q)?;
        self.check_recovery_capability()?;
        if !self.is_host() || sender.connection.as_ref().is_none_or(|b| !q.is_bound_to(b)) {
            return Err(Error::ForeignConnection);
        }
        let mut expected = sender.view.ok_or(Error::InvalidRoutes)?;
        expected.recovery = expected.recovery.next().ok_or(Error::InvalidRoutes)?;
        let subscription = sender.egress.stream_subscription().map_err(Error::Media)?;
        if !same_view(expected, self.binding()) || !subscription.recovery_pending(self.limits) {
            return Err(Error::InvalidRoutes);
        }
        let until = subscription.recovery_deadline().map_err(Error::Media)?;
        let setup = self
            .decoder_setup(q, Duration::from_secs(2))
            .map_err(|_| Error::InvalidRoutes)?
            .capped_at(until);
        let routes = Routes::new(
            self.bindings,
            self.video.outbound,
            self.recovery.outbound,
            self.video.datagram.ok_or(Error::InvalidRoutes)?,
            self.video.inbound,
        )?;
        // Subscription::recover checks the original failure deadline before
        // replacement and preserves its chronic-viewer/repair histories.
        sender
            .egress
            .recover(self.epoch(), self.bindings)
            .map_err(Error::Media)?;
        sender.routes = routes;
        sender.view = Some(self.binding());
        Ok(setup)
    }
}

impl QuicEgress {
    pub(crate) fn schedule_recovery(
        &self,
        q: &QuicRecords,
        source: &mut CaptureSource,
        demand: RecoveryDemand,
    ) -> Result<(), Error> {
        if self.connection.as_ref().is_none_or(|b| !q.is_bound_to(b)) || q.is_closed() {
            return Err(Error::ForeignConnection);
        }
        self.egress
            .stream_subscription()
            .map_err(Error::Media)?
            .schedule_recovery(source, demand)
            .map_err(Error::Media)
    }
}
