//! Original-owner admission for an asynchronous native clipboard observation.
use super::{
    CancelReason, ChannelSession, Error, HostDuration, HostInstant, Offer, SessionError, Stamp,
};
use core::fmt;

/// Exclusive borrow of the admitted channel, not a transferable permission.
/// No input-authority mutex is held across native work. The borrow prevents
/// other channel calls from consuming an off/on transition while this read is
/// pending. Drop abandons the observation; it never publishes or queues text.
pub struct Observation<'a> {
    channel: &'a mut ChannelSession,
    deadline: HostInstant,
    finished: bool,
}
impl fmt::Debug for Observation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardObservation")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}
impl ChannelSession {
    /// Admit observation before reading the OS. Hold this unique borrow until
    /// completion/cancellation and call `check` during silence and after native
    /// calls. A new grant, lease renewal or off/on cycle cannot extend the read.
    /// This does not open/read a clipboard or fabricate viewer authority.
    pub fn observe(&mut self, now: HostInstant) -> Result<Observation<'_>, SessionError> {
        if !self.maintain(now)?.enabled {
            return Err(Error::Disabled.into());
        }
        let authority_deadline = self.monitor.deadline(now).map_err(|reason| {
            self.close();
            SessionError::Clipboard(Error::Authority(reason))
        })?;
        let deadline = now
            .checked_add(HostDuration::from_micros(3_000_000))
            .ok_or_else(|| {
                self.close();
                SessionError::Clipboard(Error::Clock)
            })?
            .min(authority_deadline);
        Ok(Observation {
            channel: self,
            deadline,
            finished: false,
        })
    }
}
impl Observation<'_> {
    pub const fn deadline(&self) -> HostInstant {
        self.deadline
    }
    /// Any failure permanently consumes this observation, even when the channel
    /// can remain usable (for example after a disabled switch is enabled again).
    /// The native owner must discard its partial bytes when this returns Err.
    pub fn check(&mut self, now: HostInstant) -> Result<(), SessionError> {
        if self.finished {
            return Err(Error::Closed.into());
        }
        // Fail closed even if a future authority implementation unwinds.
        self.finished = true;
        if !self.channel.maintain(now)?.enabled {
            return Err(Error::Disabled.into());
        }
        if now >= self.deadline {
            return Err(Error::Expired.into());
        }
        self.finished = false;
        Ok(())
    }
    /// Report the native change BEFORE reading its text. Genuine changes fence
    /// an incomplete inbound publication and retire the older outbound item,
    /// even if the new selection is unavailable/unsupported or the read fails.
    /// Exact current provenance suppresses our own publication notification.
    pub fn native_change(
        &mut self,
        origin: Option<Stamp>,
        now: HostInstant,
    ) -> Result<(), SessionError> {
        self.check(now)?;
        self.finished = true;
        let changed = self
            .channel
            .receiver
            .local_change(origin, now)
            .map_err(|error| self.channel.clipboard_error(error))?;
        if changed {
            self.channel.discard_pending(CancelReason::Superseded);
        }
        self.finished = false;
        Ok(())
    }
    /// Complete exactly once with a CURRENT, fully validated native selection.
    /// Native ownership/revision checks remain the adapter's responsibility.
    /// Only `ChannelSession::pump` can admit encoded bytes to the transport;
    /// `Queued` is not proof of publication, remote paste, or network delivery.
    pub fn finish(
        &mut self,
        id: u128,
        text: &str,
        origin: Option<Stamp>,
        now: HostInstant,
    ) -> Result<Offer, SessionError> {
        self.check(now)?;
        self.finished = true;
        self.channel.offer(id, text, origin, now)
    }
}
