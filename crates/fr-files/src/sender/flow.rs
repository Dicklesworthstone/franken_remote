//! Local flow control on the original owner; no wire pause, replay or new grant.
use super::{Error, Sender, Transfer};
use fr_transport::quic::QuicRecords;

impl Sender<'_> {
    /// Defer new file records and the next queued batch source. This does not
    /// suspend desktop traffic, drain already-admitted bytes retroactively, or
    /// interrupt an outstanding bounded disk operation. A running preparation
    /// may finish, but no new source read is requested while paused.
    ///
    /// Continue calling `service` and driving the original connection: receipts,
    /// cancellation, authority checks, cleanup and ALL original deadlines remain
    /// live. In particular, a packet's existing expiry is never renewed. Resuming
    /// expresses local intent only; ordinary service rechecks current authority
    /// before any data or completion record can enter the channel.
    ///
    /// The protocol stage and transfer IDs do not change. Setting this on an
    /// idle sender also gates a subsequently queued batch, without starting it.
    pub fn set_paused(&mut self, q: &QuicRecords, paused: bool) -> Result<(), Error> {
        self.identity(q)?;
        if self.closed {
            return Err(Error::Closed);
        }
        self.lane.check(q).map_err(Error::Transport)?;
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        self.paused = paused;
        Ok(())
    }

    /// Local admission gate, not evidence that already-admitted bytes stopped
    /// or that the peer rolled back anything. `stage` retains its protocol meaning.
    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

impl Transfer {
    pub(super) fn pause_turn(&self, now: u64) -> Result<(), Error> {
        // A backpressured but unadmitted packet keeps its original expiry too.
        // Check after dispatch so a received terminal proof/refusal still wins.
        if self
            .packet
            .as_ref()
            .is_some_and(|packet| now >= packet.deadline)
        {
            Err(Error::Expired)
        } else {
            Ok(())
        }
    }
}
