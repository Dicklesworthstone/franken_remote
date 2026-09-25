//! Control responses belong to this exact input grant and its view lifetime.
use super::{ClientInstant, Error, InputClient, StopReason};
use crate::authority::{self, ObservationResponder};
use fr_wire::authority::Binding;

impl InputClient {
    /// Attach once to the authenticated session-control channel. This does not
    /// grant control or make the view ready; the lease comes from this owner.
    pub fn enable_control_renewal(
        &mut self,
        channel: u32,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        if self.control_response.is_some() {
            return Err(Error::InvalidConfiguration);
        }
        self.control_response = Some(
            ObservationResponder::for_control(
                Binding {
                    channel,
                    session: self.credentials.session,
                },
                self.credentials.lease,
                self.limits,
                now,
            )
            .map_err(Error::Control)?,
        );
        Ok(())
    }
    fn control_ready(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if !self.mapped {
            return Err(Error::MappingUnconfirmed);
        }
        if self.view_until.is_none() {
            return Err(Error::NoPresentedView);
        }
        Ok(())
    }
    /// The session retains a backpressured challenge unread. A response never
    /// refreshes source evidence or a ticket. Ticket-only expiry is not lease
    /// loss, so it does not prevent renewal of otherwise-live control.
    pub fn accept_control_challenge(
        &mut self,
        bytes: &[u8],
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.control_ready(now)?;
        let result = self
            .control_response
            .as_mut()
            .ok_or(Error::InvalidConfiguration)?
            .accept(bytes, now);
        if let Err(error) = result {
            if error != authority::Error::Backpressure {
                self.stop(StopReason::InvalidControl);
            }
            return Err(Error::Control(error));
        }
        self.clipboard_readiness();
        Ok(())
    }
    pub fn control_response_deadline(&self) -> Option<ClientInstant> {
        self.control_response
            .as_ref()
            .and_then(ObservationResponder::response_deadline)
    }
    /// Recheck current view/lifecycle immediately before admitting these exact
    /// bytes to transport. A new frame or ticket cannot restart their deadline.
    pub fn pending_control_response(&mut self, now: ClientInstant) -> Result<Option<&[u8]>, Error> {
        self.control_ready(now)?;
        self.control_response
            .as_mut()
            .ok_or(Error::InvalidConfiguration)?
            .pending(now)
            .map_err(Error::Control)
    }
    pub fn control_response_sent(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.control_ready(now)?;
        let result = self
            .control_response
            .as_mut()
            .ok_or(Error::InvalidConfiguration)?
            .sent(now);
        if let Err(error) = result {
            self.stop(StopReason::InvalidControl);
            return Err(Error::Control(error));
        }
        self.clipboard_readiness();
        Ok(())
    }
}

impl super::presentation::PresentedInput {
    /// Consume a host notification on the ORIGINAL authenticated, reliable
    /// session-control route supplied by the coordinator, not the input route.
    /// Revocation is terminal even without a fresh view or usable ticket; do
    /// not call tick here or let renewal backpressure hide this notification.
    /// A mismatched session/lease does not mutate this owner. The caller decides
    /// how to close a connection that supplied an invalid record.
    pub fn accept_lease_revoked(
        &mut self,
        bytes: &[u8],
        control: Binding,
    ) -> Result<fr_wire::lease_revoked::Revoked, super::presentation::Error> {
        let input = self.binding();
        if control.session != input.session {
            return Err(Error::Wire(fr_wire::WireError::InvalidBinding).into());
        }
        let revoked = fr_wire::lease_revoked::decode(
            bytes,
            control,
            input.lease,
            &self.protocol_limits(),
            fr_wire::input::InputDirection::HostToViewer,
            fr_wire::input::InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        // Disconnect this grant, not its pending receipt ledger. Return the
        // precise host reason/stages rather than guessing from the local stop.
        self.stop(StopReason::Disconnected);
        Ok(revoked)
    }
}
