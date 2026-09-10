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
        Ok(())
    }
}
