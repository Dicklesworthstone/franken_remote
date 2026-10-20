//! Ticket delivery updates future actions only. Receipts and consumed action
//! identities survive renewal and expiry; old actions are never re-encoded.
use super::{ClientInstant, Error, InputClient, StopReason};
use fr_core::ids::{HostBootId, RecoveryGeneration};
use fr_media::freshness::ClockCorrelation;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    input_ticket,
    presented::MAX_SOURCE_AGE_US,
};
#[derive(Clone, Copy)]
pub(super) struct State {
    pub until_us: u64,
    sequence: u64,
    pub(super) boot: HostBootId,
    issued_at_us: u64,
    recovery: RecoveryGeneration,
}
impl InputClient {
    /// `clock` must come from this authenticated session's clock exchange. The
    /// record must arrive on its installed reliable input feedback route.
    /// Expiry pauses new input; a timely new ticket can recover only a still-live,
    /// fresh view. This cannot reopen any terminal lifecycle or input failure.
    /// During a recovery, in-order tickets still naming a retired generation
    /// are consumed as obsolete feedback, never installed as current authority.
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
        self.install_ticket(ticket, None, None, clock, now)
    }
    /// Accept the explicitly negotiated authority-bearing ticket profile on
    /// the original reliable feedback route. The added deadline reports the
    /// host's existing lease/observation authority for auxiliary operations;
    /// it never extends this ticket's input expiry or proves view readiness.
    /// Old clipboard/file items keep their original immutable deadlines.
    pub fn accept_authority_ticket(
        &mut self,
        bytes: &[u8],
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        let record = input_ticket::decode_authority(
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
        self.install_ticket(
            record.ticket,
            Some(record.authority_until_us),
            None,
            clock,
            now,
        )
    }
    /// Accept only the explicitly selected causal readiness profile. Its
    /// acknowledged presentation report must belong to this original reporter,
    /// and cross this suspension's admitted negative barrier before resumption.
    pub fn accept_readiness_ticket(
        &mut self,
        bytes: &[u8],
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        let record = input_ticket::decode_readiness(
            bytes,
            &self.limits,
            self.binding.channel,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(|error| {
            self.stop(StopReason::InvalidTicket);
            Error::Wire(error)
        })?;
        self.install_ticket(
            record.authority.ticket,
            Some(record.authority.authority_until_us),
            Some(record.presented_sequence),
            clock,
            now,
        )
    }
    fn install_ticket(
        &mut self,
        ticket: input_ticket::Ticket,
        authority_until: Option<u64>,
        presented_sequence: Option<u64>,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        if presented_sequence.is_some() != self.readiness.is_some() {
            return self.fail(StopReason::InvalidTicket);
        }
        let c = ticket.credentials;
        // The reliable ticket stream is ordered independently of media
        // replacement. A ticket queued before the host's recovery fence may
        // arrive after this viewer has installed the recovered receiver. Drain
        // it without reviving the retired view or killing the surviving lease.
        let mut same_view = c.view;
        same_view.recovery = self.credentials.view.recovery;
        let obsolete = self.awaiting_recovery_ticket
            && same_view == self.credentials.view
            && self.credentials.view.recovery.supersedes(c.view.recovery)
            && self
                .ticket_state
                .is_some_and(|s| c.view.recovery >= s.recovery);
        if c.session != self.credentials.session
            || c.lease != self.credentials.lease
            || (c.view != self.credentials.view && !obsolete)
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
        if obsolete {
            // Only replay metadata changes. In particular, neither deadline,
            // ticket identity, held state, clipboard nor suspension is revived.
            let state = self
                .ticket_state
                .as_mut()
                .expect("validated obsolete ticket");
            state.sequence = ticket.sequence;
            state.issued_at_us = ticket.issued_at_us;
            state.recovery = c.view.recovery;
            return Ok(());
        }
        // Record the consumed sequence even for late delivery. It cannot be
        // replayed after a later clock sample and be given a new lease on life.
        self.ticket_state = Some(State {
            until_us: until,
            sequence: ticket.sequence,
            boot: clock.host_boot(),
            issued_at_us: ticket.issued_at_us,
            recovery: c.view.recovery,
        });
        if until <= now.0 {
            return Err(Error::TicketExpired);
        }
        let causal_ready = if let Some(sequence) = presented_sequence {
            match self
                .readiness
                .as_mut()
                .expect("selected profile")
                .acknowledge(sequence)
            {
                Ok(ready) => ready,
                Err(()) => return self.fail(StopReason::InvalidTicket),
            }
        } else {
            false
        };
        self.credentials.ticket = c.ticket;
        if self.awaiting_recovery_ticket {
            // Unlike a same-generation freshness lapse, this exact new view
            // could not have had a ticket before the host installed recovery.
            // Its timely, authenticated ticket proves that fence was crossed;
            // waiting another source-age ceiling only delays usable control.
            // Fresh presentation remains an independent prerequisite.
            self.awaiting_recovery_ticket = false;
            if self.readiness.is_none() || causal_ready {
                self.awaiting_ticket = false;
            }
            self.try_resume(now);
        } else if let Some(since) = self.suspended_since {
            // Only a ticket issued after the HOST's own lapse can authorize
            // input again: the host invalidated every older one. That lapse
            // is at most the 1 s source-age ceiling after this suspension
            // began, so a ticket conservatively issued later than that was
            // issued after the host revived (it refuses tickets meanwhile).
            let elapsed = now.0.saturating_sub(since.0);
            if causal_ready
                || (self.readiness.is_none()
                    && clock
                        .age_upper_us(ticket.issued_at_us, now.0)
                        .is_ok_and(|age| age.saturating_add(MAX_SOURCE_AGE_US) < elapsed))
            {
                self.awaiting_ticket = false;
                self.try_resume(now);
            }
        }
        self.clipboard_readiness();
        self.auxiliary_ticket(ticket.expires_at_us, authority_until, clock, now);
        Ok(())
    }
    /// Only the input owner's validated same-lease ticket extends auxiliary
    /// proof. Each lane keeps independent lifetime and one-use attachment state.
    fn auxiliary_ticket(
        &mut self,
        host_until: u64,
        authority_until: Option<u64>,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) {
        let resumed = !self.awaiting_ticket && !self.awaiting_recovery_ticket;
        for projection in [&mut self.clipboard_projection, &mut self.files_projection] {
            if projection.as_ref().is_some_and(|owner| {
                match authority_until {
                    Some(until) => owner.authority_ticket(until, clock, now, resumed),
                    None => owner.ticket(host_until, clock, now, resumed),
                }
                .is_err()
            }) {
                drop(projection.take());
            }
        }
    }
    pub fn ticket_deadline(&self) -> Option<ClientInstant> {
        if self.awaiting_recovery_ticket {
            None
        } else {
            self.ticket_state.map(|s| ClientInstant(s.until_us))
        }
    }
}
