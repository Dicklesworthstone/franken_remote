//! Ticket issuance shares the one native mailbox and feedback slot. No timer,
//! lease grant, RNG, queue or input replay is introduced here.
use super::{Command, Error, Feedback, IoGuard, Messages, Pending, Progress, QuicInput, Reply};
use fr_core::ids::InputTicketId;
use fr_transport::quic::QuicRecords;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    input_ticket::{
        self, AuthorityTicket, Profile, READINESS_TICKET_BYTES, ReadinessTicket, Ticket,
    },
};

pub const TICKET_CADENCE_US: u64 = 250_000;
enum Record {
    Legacy(Ticket),
    Authority(AuthorityTicket),
    Readiness(ReadinessTicket),
}
impl Record {
    const fn ticket(&self) -> Ticket {
        match self {
            Self::Legacy(ticket) => *ticket,
            Self::Authority(record) => record.ticket,
            Self::Readiness(record) => record.authority.ticket,
        }
    }
    fn encode(
        self,
        bytes: &mut [u8],
        limits: &fr_core::limits::ProtocolLimits,
        binding: u32,
    ) -> Result<usize, fr_wire::WireError> {
        match self {
            Self::Legacy(ticket) => input_ticket::encode(
                ticket,
                bytes,
                limits,
                binding,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            ),
            Self::Authority(record) => input_ticket::encode_authority(
                record,
                bytes,
                limits,
                binding,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            ),
            Self::Readiness(record) => input_ticket::encode_readiness(
                record,
                bytes,
                limits,
                binding,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            ),
        }
    }
}
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
                match self.ticket_profile {
                    Profile::Legacy => self.agent.issue_ticket_record(ticket, sequence),
                    Profile::Authority => {
                        self.agent.issue_authority_ticket_record(ticket, sequence)
                    }
                    Profile::Readiness => {
                        self.agent.issue_readiness_ticket_record(ticket, sequence)
                    }
                }
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
        let result = match (self.ticket_profile, reply) {
            (Profile::Legacy, Reply::Ticket(result)) => result.map(Record::Legacy),
            (Profile::Authority, Reply::AuthorityTicket(result)) => result.map(Record::Authority),
            (Profile::Readiness, Reply::ReadinessTicket(result)) => result.map(Record::Readiness),
            _ => return Err(Error::Agent(crate::input_agent::Error::NotInputCommand)),
        };
        let record = match result {
            Ok(record) => record,
            // A stale view suspends the lease (plan 11.3): no ticket this turn.
            // The next cadence retries once fresh readiness revives it.
            Err(fr_core::input_submission::Refusal::Authority(
                fr_core::authority::AuthorityError::ViewUnready,
            )) => return Ok(Progress::Idle),
            Err(refusal) => return Err(Error::TicketRefused(refusal)),
        };
        let ticket = record.ticket();
        let mut bytes = [0; READINESS_TICKET_BYTES];
        let length = record
            .encode(&mut bytes, &self.limits, self.routes.results.binding)
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
