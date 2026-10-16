//! Preserve an idle control-capable observation across reference recovery.
//! A pending request or native grant is never rewritten to another target.
use super::{Error, GrantBroker};
use crate::media_quic::NegotiatedMedia;
use fr_transport::quic::QuicRecords;
use fr_wire::decoder::Binding;

impl GrantBroker {
    /// The broker's current configuration comes from its retained attachment,
    /// not the request's numeric display ID. Unavailable during replacement.
    pub(crate) fn configuration(&mut self, q: &QuicRecords) -> Result<Option<Binding>, Error> {
        self.bound(q)?;
        self.live(q)?;
        if self.recovery_until.is_some() {
            return Ok(None);
        }
        self.negotiated
            .as_ref()
            .map(|input| input.host_scope(q).map(|(_, binding)| binding))
            .transpose()
            .map_err(Error::Input)
    }
    pub(crate) fn begin_reference_recovery(
        &mut self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
        until: u64,
    ) -> Result<(), Error> {
        self.bound(q)?;
        let now = self.live(q)?;
        if self.waiting.is_some() || self.recovery_until.is_some() {
            return Err(Error::RequestPending);
        }
        if self.native.is_some() {
            return Err(Error::AlreadyGranted);
        }
        if now >= until {
            return Err(Error::Expired);
        }
        media.check(q).map_err(|_| Error::InvalidRoutes)?;
        let input = self.negotiated.as_mut().ok_or(Error::NotNegotiated)?;
        if input.host_scope(q).map_err(Error::Input)? != (self.parent, media.binding()) {
            return Err(Error::TargetChanged);
        }
        input.begin_host_recovery(q).map_err(Error::Input)?;
        self.recovery_until = Some(until);
        Ok(())
    }
    pub(crate) fn finish_reference_recovery(
        &mut self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
    ) -> Result<(), Error> {
        self.bound(q)?;
        self.live(q)?;
        let until = self.recovery_until.ok_or(Error::InvalidRoutes)?;
        self.negotiated
            .as_mut()
            .ok_or(Error::NotNegotiated)?
            .follow_host_recovery(q, media, until)
            .map_err(Error::Input)?;
        self.recovery_until = None;
        // Completing a handoff cannot extend its original budget. Failure
        // fences the broker even if its final clock read raced the deadline.
        if self.live(q).inspect_err(|_| self.stop())? >= until {
            self.stop();
            return Err(Error::Expired);
        }
        Ok(())
    }
}
