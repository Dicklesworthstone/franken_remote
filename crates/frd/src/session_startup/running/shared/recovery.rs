//! Bounded recovery turns inside the original session renewal/UDP owner.
use super::{
    Error, HostFeedback, HostPresentation, Services, Setup, SharedServices, feedback, presented,
};
use crate::media::shared_publisher::RecoveryState;
use fr_transport::quic::{Disposition, QuicRecords, Route};
use fr_wire::{
    attachment::Ticket,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    recovery_request,
};

/// Retain only the old protocol scope, never pixels, a new grant or another clock.
pub(super) struct Handoff {
    previous: Binding,
    repair: Route,
}
fn is_request(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&0x0036_u16.to_be_bytes())
}
impl<F: FnMut(Route, &[u8]) -> Result<Disposition, ()>> SharedServices<'_, F> {
    pub(super) fn maintain_recovery(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) -> Result<(), Error> {
        if self.recovery.is_none()
            && self
                .subscriber
                .dispatch_recovery(q, self.routes, self.parent)
                .map_err(Error::SharedPublication)?
        {
            let mut previous = self
                .subscriber
                .bind_session(q, &self.control, self.parent)
                .map_err(Error::SharedPublication)?;
            previous.parent = self.parent;
            *self.recovery = Some(Handoff {
                previous,
                repair: *self.repair,
            });
            // Their outstanding timers/reports describe the failed generation.
            // Keep totals, but do not let either verifier certify its successor.
            *self.presentation = None;
            *self.feedback = None;
            self.statistics.recovery_requests = self.statistics.recovery_requests.saturating_add(1);
        }
        if self.recovery.is_none() {
            return Ok(());
        }
        let state = self
            .subscriber
            .recovery_state(q)
            .map_err(Error::SharedPublication)?;
        let tickets = if state == RecoveryState::NeedsTickets {
            let mut tickets = [Ticket(0); 3];
            for ticket in &mut tickets {
                if !self.permitted() {
                    return Err(Error::Authority);
                }
                *ticket = Ticket(nonce().map_err(|()| Error::Order)?);
                // A nonce supplier cannot prolong consent by blocking or revoking
                // the original source. Never execute it inside publisher locks.
                if !self.permitted() {
                    return Err(Error::Authority);
                }
            }
            Some(tickets)
        } else {
            None
        };
        if matches!(
            state,
            RecoveryState::NeedsTickets | RecoveryState::Attaching
        ) {
            self.subscriber
                .advance_recovery(&self.control.context(), q, tickets)
                .map_err(Error::SharedPublication)?;
        }
        // The repair owner changes once attachment completes, before decoder
        // startup. Its reports remain blocked until the first decode succeeds.
        *self.repair = self
            .subscriber
            .repair_route(q)
            .map_err(Error::SharedPublication)?;
        Ok(())
    }
    pub(super) fn finish_recovery(&mut self, q: &QuicRecords) -> Result<(), Error> {
        if self.recovery.is_none()
            || self
                .subscriber
                .recovery_state(q)
                .map_err(Error::SharedPublication)?
                != RecoveryState::Receiving
        {
            return Ok(());
        }
        let view = self
            .subscriber
            .bind_session(q, &self.control, self.parent)
            .map_err(Error::SharedPublication)?;
        let mut presentation = HostPresentation::attach(
            self.selection,
            self.parent,
            view,
            q,
            self.routes.inbound,
            self.control.clone(),
        )
        .map_err(Error::PresentedState)?;
        let mut feedback = Setup::selected(self.selection, self.parent, view)
            .map_err(Error::ReceiverFeedback)?
            .map(|setup| {
                HostFeedback::new(
                    setup,
                    Route::Stream(self.routes.inbound),
                    Route::Stream(self.routes.outbound),
                )
            })
            .transpose()
            .map_err(Error::ReceiverFeedback)?;
        if let Some(value) = &mut presentation {
            value.accepted = self.statistics.presentation_reports;
        }
        if let Some(value) = &mut feedback {
            value.accepted = self.statistics.feedback_reports;
        }
        *self.presentation = presentation;
        *self.feedback = feedback;
        *self.recovery = None;
        self.statistics.recovered_streams = self.statistics.recovered_streams.saturating_add(1);
        Ok(())
    }
    pub(super) fn recovery_record(
        &mut self,
        route: Route,
        bytes: &[u8],
    ) -> Result<Option<Disposition>, ()> {
        if !self.permitted() {
            return Err(());
        }
        if let Some(handoff) = &*self.recovery {
            if self.subscriber.owns_recovery_record(route, bytes) {
                return Ok(Some(Disposition::Blocked));
            }
            if route == Route::Stream(self.routes.inbound) && is_request(bytes) {
                // A duplicate consumes transport credit, never another recovery
                // charge or a fresh deadline. Malformed/foreign scopes still fail.
                recovery_request::decode(
                    bytes,
                    handoff.previous,
                    &self.selection.limits,
                    InputDirection::ViewerToHost,
                    InputDelivery::Reliable,
                )
                .map_err(|_| ())?;
                return Ok(Some(Disposition::Consumed));
            }
            if route == handoff.repair
                || (route == Route::Stream(self.routes.inbound)
                    && (presented::is_report(bytes) || feedback::is_feedback(bytes)))
            {
                return Ok(Some(Disposition::Consumed));
            }
        }
        if route == Route::Stream(self.routes.inbound) && is_request(bytes) {
            // A record arriving after maintain belongs to the NEXT maintain turn,
            // never an unrelated callback or a best-effort decoded request queue.
            return Ok(Some(Disposition::Blocked));
        }
        Ok(None)
    }
}
