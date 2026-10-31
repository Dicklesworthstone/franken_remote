//! Optional controller lanes keep independent tickets on shared CONTROL framing.
//! This is dispatch only: the matching owner still validates every transition.
use super::{
    ARMED, Error, InputDelivery, InputDirection, MediaChannel, MediaRole, Message, Ordering,
    PENDING, QuicRecords, attachment,
};

/// Optional owners share only the original CONTROL parser; each still owns
/// one independent, immutable ticket and stream pair. At most three siblings
/// exist (clipboard, upload, download, microphone), so dispatch allocates no queue.
pub(super) fn optional_siblings(a: MediaRole, b: MediaRole) -> bool {
    a != b && optional(a) && optional(b)
}

fn optional(role: MediaRole) -> bool {
    matches!(role, MediaRole::Clipboard | MediaRole::AudioUp) || role.is_files()
}

type PendingSiblings = [Option<(u32, MediaRole)>; 3];

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
    fn sibling_role(&self, role: MediaRole) -> bool {
        optional_siblings(self.descriptor.role, role)
            && (role != MediaRole::FilesDownload || self.files_duplex)
            && (role != MediaRole::AudioUp || self.audio_up)
    }
    pub(super) fn pending_siblings(&self, q: &QuicRecords) -> PendingSiblings {
        let mut siblings = [None; 3];
        for (slot, reservation) in siblings.iter_mut().zip(q.attachments.iter().filter(|r| {
            self.sibling_role(r.role) && matches!(r.state.load(Ordering::Acquire), PENDING | ARMED)
        })) {
            *slot = Some((reservation.binding, reservation.role));
        }
        siblings
    }
    pub(super) fn defer_sibling(
        &self,
        bytes: &[u8],
        pending: PendingSiblings,
    ) -> Result<bool, Error> {
        if !optional(self.descriptor.role) {
            return Ok(false);
        }
        // Validate the complete original-session record before classifying it.
        // An unknown ACK, malformed record or wrong tuple on our own binding
        // stays a protocol error, never an ignored message or another grant.
        let message =
            if self.credit && self.host && bytes.get(6..8) == Some(&0x001c_u16.to_be_bytes()) {
                // Sibling dispatch must use the SAME positively selected layout as
                // the owner. Validate its complete credit before deferring; only
                // the matching owner may intersect that credit with its reservation.
                let accepted = attachment::credit::decode(
                    bytes,
                    self.parent,
                    &self.limits,
                    InputDirection::ViewerToHost,
                    InputDelivery::Reliable,
                )
                .map_err(|_| Error::Malformed)?;
                Message::Accepted(accepted.binding)
            } else {
                attachment::decode(
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
                .map_err(|_| Error::Malformed)?
            };
        Ok(match message {
            // The other viewer-side acceptor has not reserved its pair yet.
            // Leave its offer in the original parser, without allocation.
            Message::Binding(descriptor) => {
                self.sibling_role(descriptor.role)
                    && descriptor.binding.parent.id != self.descriptor.binding.parent.id
            }
            Message::Accepted(id) => pending.iter().flatten().any(|(binding, _)| *binding == id),
            Message::Ticket(grant) => {
                self.sibling_role(grant.descriptor.role)
                    && pending.contains(&Some((
                        grant.descriptor.binding.parent.id,
                        grant.descriptor.role,
                    )))
            }
            _ => false,
        })
    }
}
