//! Ticket issuance shares the one native mailbox and feedback slot. No timer,
//! lease grant, RNG, queue or input replay is introduced here.
use super::{Command, Error, Feedback, IoGuard, Messages, Pending, Progress, QuicInput, Reply};
use fr_core::ids::InputTicketId;
use fr_transport::quic::QuicRecords;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    input_ticket::{self, INPUT_TICKET_BYTES, Ticket},
};

pub const TICKET_CADENCE_US: u64 = 250_000;
impl QuicInput {
    /// Call on idle turns AFTER giving ordered input a receive turn. The
    /// authenticated session must install `InputFeedback` on both endpoints.
    /// `fresh` supplies a qualified, unpredictable, never-reused host ID, and is
    /// not called while a command/result is pending or before the cadence.
    /// Transport/cancellation failures revoke; backpressure never replaces an
    /// old ticket with new bytes or re-encodes previously submitted input.
    pub fn renew_ticket(
        &mut self,
        connection: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
        fresh: impl FnOnce() -> Option<InputTicketId>,
    ) -> Result<Progress, Error> {
        self.bound(connection)?;
        if self.routes.results.messages != Messages::InputFeedback {
            return Err(Error::InvalidRoutes);
        }
        let mut io = IoGuard::new(connection, self.control());
        io.connection
            .tick(&self.cx, &mut authorize)
            .map_err(Error::Transport)?;
        if io
            .connection
            .receive_ended(self.routes.actions)
            .map_err(Error::Transport)?
            || self.control().is_stopped()
        {
            return Err(Error::Closed);
        }
        let progress = if let Some(pending) = &self.pending {
            pending.backpressure()
        } else if self.command.is_some() {
            Progress::NativePending
        } else {
            let now = self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
            if now < self.ticket_after_us {
                Progress::Idle
            } else {
                let sequence = self.ticket_sequence.ok_or(Error::InvalidTicketId)?;
                let ticket = fresh()
                    .filter(|t| t.as_raw() != 0 && Some(*t) != self.last_ticket)
                    .ok_or(Error::InvalidTicketId)?;
                let next = now.checked_add(TICKET_CADENCE_US).ok_or(Error::Clock)?;
                self.agent
                    .issue_ticket_record(ticket, sequence)
                    .map_err(Error::Agent)?;
                self.command = Some(Command::Ticket);
                self.ticket_sequence = sequence.checked_add(1);
                self.ticket_after_us = next;
                self.last_ticket = Some(ticket);
                Progress::NativePending
            }
        };
        io.complete = true;
        Ok(progress)
    }
    pub fn pending_ticket(&self) -> Option<Ticket> {
        self.pending.as_ref().and_then(|p| match p.feedback {
            Feedback::Ticket(ticket) => Some(ticket),
            Feedback::Result(_) => None,
        })
    }
    pub(super) fn collect_ticket(&mut self) -> Result<Progress, Error> {
        let Some(reply) = self.agent.try_reply().map_err(Error::Agent)? else {
            return Ok(Progress::NativePending);
        };
        self.command = None;
        let Reply::Ticket(result) = reply else {
            return Err(Error::Agent(crate::input_agent::Error::NotInputCommand));
        };
        let ticket = result.map_err(Error::TicketRefused)?;
        let mut bytes = [0; INPUT_TICKET_BYTES];
        let length = input_ticket::encode(
            ticket,
            &mut bytes,
            &self.limits,
            self.routes.results.binding,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        self.pending = Some(Pending {
            feedback: Feedback::Ticket(ticket),
            bytes,
            length,
            until: ticket.expires_at_us,
        });
        Ok(Progress::TicketBackpressure)
    }
}
