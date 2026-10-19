//! A Files source or recipient may cancel at its authenticated final handoff.
use super::{
    super::{ACTIVE, ARMED, MediaChannel, MediaRole, Ordering, Phase, QuicRecords, RETIRED},
    FILES_RETIRED,
};
use crate::quic::{Cx, Error};
use std::sync::Arc;

impl MediaChannel {
    /// Retire a peer-cancelled Files lane at its original final boundary. The
    /// host requires validated Attach and fully staged Attached. A positively
    /// selected download viewer requires the exact Ticket and fully staged
    /// Attach; RESET may discard its still-unread Attached. This is retirement,
    /// not successful attachment or receive authority. Completed attachments may
    /// already have a peer-retired tombstone. No disk receiver starts after true.
    /// Earlier phases, other roles, parent expiry and unknown codes stay strict.
    pub fn retire_files_if_peer_cancelled(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        // Foreign objects must not mutate either owner before being refused.
        if !q.is_bound_to(&self.connection)
            || (!self.host && !self.files_download)
            || self.descriptor.role != MediaRole::Files
        {
            return Err(Error::WrongRoute);
        }
        let result = (|| {
            if q.is_closed() {
                return Err(Error::Closed);
            }
            let Some(index) = self.cancelled_files_reservation(q)? else {
                return Ok(false);
            };
            self.check_parent(q, cx, &mut authorize)?;
            // q.check already retires ACTIVE pairs. The proven final ARMED
            // Files reservation takes the same native/buffer/credit cleanup.
            if !q.attachments[index].retired() {
                q.retire_reservation_at(cx, index)?;
            }
            Ok(true)
        })();
        match result {
            Ok(true) => self.close(),
            Err(_) => {
                self.close();
                q.close();
            }
            Ok(false) => {}
        }
        result
    }

    fn cancelled_files_reservation(&self, q: &QuicRecords) -> Result<Option<usize>, Error> {
        let state = self.state.load(Ordering::Acquire);
        let final_phase = matches!(
            (self.phase, state),
            (Phase::Promote, ARMED) | (Phase::Complete, ACTIVE | RETIRED)
        ) || (!self.host
            && self.files_download
            && self.phase == Phase::Attached
            && state == ARMED);
        if !final_phase {
            return Ok(None);
        }
        if self.phase != Phase::Complete
            && (!self.receive_armed || self.len != 0 || !q.send_staged(self.pair.outbound)?)
        {
            return Ok(None);
        }
        let index = q
            .attachments
            .iter()
            .position(|r| Arc::ptr_eq(&r.state, &self.state))
            .ok_or(Error::WrongRoute)?;
        let streams = q
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .inner()
            .streams();
        let outgoing = streams
            .stream(self.pair.outbound.stream)
            .map_err(|_| Error::Native)?;
        let incoming = streams
            .stream(self.pair.inbound.stream)
            .map_err(|_| Error::Native)?;
        let reset = incoming.recv_reset.map(|(code, _)| code);
        let stop = outgoing.stop_sending_error_code;
        if !files_retirement_requested(reset, stop) {
            return Ok(None);
        }
        // This final exchange is bound to the original validated ticket. Any
        // unread ACK or data belongs only to this spent lane: retirement charges
        // RESET's final size without parsing it or starting a disk receiver.
        Ok(Some(index))
    }
}
fn files_retirement_requested(reset: Option<u64>, stop: Option<u64>) -> bool {
    matches!(
        (reset, stop),
        (Some(FILES_RETIRED), None | Some(FILES_RETIRED)) | (None, Some(FILES_RETIRED))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_files_cancellation_code_can_retire_final_setup() {
        assert!(!files_retirement_requested(None, None));
        assert!(files_retirement_requested(Some(FILES_RETIRED), None));
        assert!(files_retirement_requested(None, Some(FILES_RETIRED)));
        assert!(files_retirement_requested(
            Some(FILES_RETIRED),
            Some(FILES_RETIRED)
        ));
        for wrong in [0, FILES_RETIRED - 1, FILES_RETIRED + 1, (1 << 62) - 1] {
            assert!(!files_retirement_requested(Some(wrong), None));
            assert!(!files_retirement_requested(None, Some(wrong)));
            assert!(!files_retirement_requested(
                Some(FILES_RETIRED),
                Some(wrong)
            ));
            assert!(!files_retirement_requested(
                Some(wrong),
                Some(FILES_RETIRED)
            ));
        }
    }
}
