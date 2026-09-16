//! Real native selection -> original authority -> bounded clipboard sender.
//!
//! This runs on the interactive native worker. It installs no listener, runtime
//! or ambient clipboard monitor. The caller supplies the admitted channel and
//! reports actual current local changes (not repeated stale notifications).
//! During a pending read the clipboard lane is exclusively borrowed; unrelated
//! input/media and the independent authority/off switches remain serviceable.
#![forbid(unsafe_code)]
use crate::clipboard::{ReadError, X11Clipboard};
use core::fmt;
use fr_core::time::HostInstant;
use fr_wire::{
    WireError,
    clipboard::session::{ChannelSession, Offer, SessionError, observation::Observation},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    Native(ReadError),
    Session(SessionError),
}
impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for CopyError {}
impl From<ReadError> for CopyError {
    fn from(error: ReadError) -> Self {
        Self::Native(error)
    }
}
impl From<SessionError> for CopyError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

/// One native read, tied to one unique live channel. No clipboard bytes escape
/// to the caller; only a completed, reauthorized selection enters its sender.
/// Drop, errors and unwinding cancel the native read without overwriting the OS
/// clipboard or touching unrelated input/media. Never clone or rebind a lease.
pub struct ChannelRead<'a> {
    clipboard: &'a mut X11Clipboard,
    observation: Observation<'a>,
    id: u128,
    finished: bool,
}
impl fmt::Debug for ChannelRead<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeClipboardChannelRead")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}
impl X11Clipboard {
    /// Read only after original-owner, negotiated-grant and both-switch checks.
    /// `id` comes from qualified randomness, never from clipboard content. The
    /// supplied clock is the admitted owner's qualified clock, not peer time.
    /// Creation and every poll recheck after native work, outside the authority
    /// mutex. The X server remains a trusted local OS boundary.
    pub fn read_to_channel<'a>(
        &'a mut self,
        channel: &'a mut ChannelSession,
        id: u128,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<ChannelRead<'a>, CopyError> {
        let mut observation = channel.observe(clock())?;
        let origin = self.current_origin();
        // A reported real local change retires older wire data even when its
        // native owner is unavailable or the caller cannot supply a valid ID.
        observation.native_change(origin.as_ref().ok().copied().flatten(), clock())?;
        origin.map_err(ReadError::from)?;
        if id == 0 {
            return Err(SessionError::Wire(WireError::InvalidValue).into());
        }
        // A Busy refusal must not cancel someone else's existing native read.
        self.begin_read()?;
        let mut read = ChannelRead {
            clipboard: self,
            observation,
            id,
            finished: false,
        };
        // The guard now owns cleanup, including a caught clock panic.
        read.observation.check(clock())?;
        Ok(read)
    }
}
impl ChannelRead<'_> {
    /// At most one native property chunk. None means pending, not empty text.
    /// The fixed observation deadline is not refreshed by progress or renewal.
    /// Any failure consumes the read; stale bytes cannot be retried after a new
    /// enable/grant. Completion offers once; later calls return `NotReading`.
    pub fn poll(
        &mut self,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Option<Offer>, CopyError> {
        if self.finished {
            return Err(ReadError::NotReading.into());
        }
        self.finished = true;
        let turn = PollGuard(self);
        let result = turn.0.poll_inner(&mut clock);
        if matches!(result, Ok(None)) {
            turn.0.finished = false;
        }
        result
    }
    fn poll_inner(
        &mut self,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<Option<Offer>, CopyError> {
        self.observation.check(clock())?;
        let text = self.clipboard.poll_read()?;
        if let Some(text) = text {
            self.observation
                .finish(self.id, text.as_str(), text.origin(), clock())
                .map(Some)
                .map_err(Into::into)
        } else {
            self.observation.check(clock())?;
            Ok(None)
        }
    }
}
impl Drop for ChannelRead<'_> {
    fn drop(&mut self) {
        self.clipboard.cancel_read();
    }
}

// A caller may catch a panic around poll while retaining the guard. Retire the
// native requestor and clear partial bytes immediately, not only on final Drop.
struct PollGuard<'a, 'b>(&'a mut ChannelRead<'b>);
impl Drop for PollGuard<'_, '_> {
    fn drop(&mut self) {
        if self.0.finished {
            self.0.clipboard.cancel_read();
        }
    }
}
