//! Recovery retains the admitted subscription instead of minting a new cache.
use super::{Error, NegotiatedMedia, QuicEgress, Routes, same_view};
use crate::media::{CaptureSource, decoder_startup::Setup};
use asupersync::net::quic_native::StreamRole;
use fr_transport::quic::{ControlRoutes, QuicRecords, Route};
use fr_wire::negotiation::{ControlBinding, Role};
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
        if self.selection.role != Role::Observe || !same_view(previous, self.binding()) {
            return Err(Error::InvalidRoutes);
        }
        self.decoder_setup(q, std::time::Duration::from_secs(2))
            .map(|setup| setup.capped_at(until_micros))
            .map_err(|_| Error::InvalidRoutes)
    }

    /// Route only the original negotiated control lane to its actual capture
    /// owner. Media-channel numbers and peer-proposed view tuples are not
    /// authority. An input-owning session needs explicit reacquisition and is
    /// refused here; recovery never silently keeps its old control lease.
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
        self.check(q)?;
        self.check_recovery_capability()?;
        if self.selection.role != Role::Observe
            || q.role().map_err(Error::Transport)? != StreamRole::Server
            || route != Route::Stream(control.inbound)
            || sender.view != Some(self.binding())
            || sender.connection.as_ref().is_none_or(|b| !q.is_bound_to(b))
        {
            return Err(Error::InvalidRoutes);
        }
        let view = super::super::recovery::control_binding(
            q,
            control,
            parent,
            self.binding(),
            *self.limits.protocol(),
        )
        .map_err(|_| Error::InvalidRoutes)?;
        sender
            .egress
            .request_recovery(source, bytes, view)
            .map_err(Error::Media)
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
        if !self.is_host()
            || self.selection.role != Role::Observe
            || sender.connection.as_ref().is_none_or(|b| !q.is_bound_to(b))
        {
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
