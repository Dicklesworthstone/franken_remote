//! Clipboard and files keep independent tickets while sharing CONTROL framing.
//! This is dispatch only: the matching owner still validates every transition.
use super::{
    ARMED, Error, InputDelivery, InputDirection, MediaChannel, MediaRole, Message, Ordering,
    PENDING, QuicRecords, attachment,
};

impl MediaChannel {
    /// Stream identifiers are reserved monotonically. Announce those pairs in
    /// that same order, even when role dispatch polls the later owner first.
    /// There is no new queue or waiting budget: ordinary backpressure and the
    /// original two-second attachment deadline still bound progress.
    pub(super) fn earlier_offer_pending(&self, q: &QuicRecords) -> bool {
        q.attachments.iter().any(|reservation| {
            reservation.binding < self.descriptor.binding.parent.id
                && reservation.state.load(Ordering::Acquire) == PENDING
                && !reservation.offered
        })
    }
    pub(super) fn mark_offered(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        let Some(reservation) = q
            .attachments
            .iter_mut()
            .find(|reservation| reservation.binding == self.descriptor.binding.parent.id)
        else {
            self.close();
            q.close();
            return Err(Error::WrongRoute);
        };
        reservation.offered = true;
        Ok(())
    }
    fn sibling_role(&self) -> Option<MediaRole> {
        match self.descriptor.role {
            MediaRole::Clipboard => Some(MediaRole::Files),
            MediaRole::Files => Some(MediaRole::Clipboard),
            _ => None,
        }
    }
    pub(super) fn pending_sibling(&self, q: &QuicRecords) -> Option<u32> {
        let role = self.sibling_role()?;
        q.attachments.iter().find_map(|reservation| {
            (reservation.role == role
                && matches!(reservation.state.load(Ordering::Acquire), PENDING | ARMED))
            .then_some(reservation.binding)
        })
    }
    pub(super) fn defer_sibling(&self, bytes: &[u8], pending: Option<u32>) -> Result<bool, Error> {
        let Some(sibling) = self.sibling_role() else {
            return Ok(false);
        };
        // Validate the complete original-session record before classifying it.
        // An unknown ACK, malformed record or wrong tuple on our own binding
        // stays a protocol error, never an ignored message or another grant.
        let message = attachment::decode(
            bytes,
            self.parent,
            self.parent.id,
            &self.limits,
            if self.host {
                InputDirection::ViewerToHost
            } else {
                InputDirection::HostToViewer
            },
            InputDelivery::Reliable,
        )
        .map_err(|_| Error::Malformed)?;
        Ok(match message {
            // The other viewer-side acceptor has not reserved its pair yet.
            // Leave its offer in the original parser, without allocation.
            Message::Binding(descriptor) => {
                descriptor.role == sibling
                    && descriptor.binding.parent.id != self.descriptor.binding.parent.id
            }
            Message::Accepted(id) => pending == Some(id),
            Message::Ticket(grant) => {
                grant.descriptor.role == sibling
                    && pending == Some(grant.descriptor.binding.parent.id)
            }
            _ => false,
        })
    }
}
