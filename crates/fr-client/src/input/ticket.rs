//! Ticket delivery updates future actions only. Receipts and consumed action
//! identities survive renewal and expiry; old actions are never re-encoded.
use super::{ClientInstant, Error, InputClient, StopReason};
use fr_core::ids::HostBootId;
use fr_media::freshness::ClockCorrelation;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    input_ticket,
};
#[derive(Clone, Copy)]
pub(super) struct State {
    pub until_us: u64,
    sequence: u64,
    boot: HostBootId,
    issued_at_us: u64,
}
impl InputClient {
    /// `clock` must come from this authenticated session's clock exchange. The
    /// record must arrive on its installed reliable input feedback route.
    /// Expiry pauses new input; a timely new ticket can recover only a still-live,
    /// fresh view. This cannot reopen any terminal lifecycle or input failure.
    pub fn accept_ticket(
        &mut self,
        bytes: &[u8],
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        let ticket = input_ticket::decode(
            bytes,
            &self.limits,
            self.binding.channel,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(|e| {
            self.stop(StopReason::InvalidTicket);
            Error::Wire(e)
        })?;
        let c = ticket.credentials;
        if c.session != self.credentials.session
            || c.lease != self.credentials.lease
            || c.view != self.credentials.view
            || (self.ticket_state.is_some() && c.ticket == self.credentials.ticket)
            || self.ticket_state.is_some_and(|s| {
                ticket.sequence <= s.sequence
                    || ticket.issued_at_us < s.issued_at_us
                    || clock.host_boot() != s.boot
            })
        {
            return self.fail(StopReason::InvalidTicket);
        }
        if clock.age_upper_us(ticket.issued_at_us, now.0).is_err() {
            return self.fail(StopReason::InvalidTicket);
        }
        let Ok(until) = clock.deadline_lower_us(ticket.expires_at_us, now.0) else {
            return self.fail(StopReason::InvalidTicket);
        };
        // Record the consumed sequence even for late delivery. It cannot be
        // replayed after a later clock sample and be given a new lease on life.
        self.ticket_state = Some(State {
            until_us: until,
            sequence: ticket.sequence,
            boot: clock.host_boot(),
            issued_at_us: ticket.issued_at_us,
        });
        if until <= now.0 {
            return Err(Error::TicketExpired);
        }
        self.credentials.ticket = c.ticket;
        Ok(())
    }
    pub fn ticket_deadline(&self) -> Option<ClientInstant> {
        self.ticket_state.map(|s| ClientInstant(s.until_us))
    }
}
