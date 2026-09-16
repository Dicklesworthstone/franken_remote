//! Coalesced server-authored `XFixes` notifications, never content polling.
use super::{PlatformError, Stamp, X11Clipboard, ffi};
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchError {
    Platform(PlatformError),
    Unsupported,
    NotWatching,
    AlreadyWatching,
    GenerationExhausted,
}
impl From<PlatformError> for WatchError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}
impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for WatchError {}

/// A native revision, not an authorization to read or disclose. Opaque native
/// identities are deliberately absent from Debug. Even a same-owner/same-time
/// reselection is a new change, not content-based deduplication.
#[derive(Clone, Copy)]
pub struct ClipboardChange {
    revision: u64,
    owner: u32,
    time: u32,
    origin: Option<Stamp>,
}
impl ClipboardChange {
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    pub const fn has_selection(&self) -> bool {
        self.owner != 0
    }
    pub const fn origin(&self) -> Option<Stamp> {
        self.origin
    }
}
impl fmt::Debug for ClipboardChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardChange([redacted])")
    }
}
#[derive(Default)]
pub(super) struct Changes {
    active: bool,
    revision: u64,
    exhausted: bool,
    pending: Option<ClipboardChange>,
}
impl Changes {
    pub(super) fn record(&mut self, owner: u32, time: u32) {
        if !self.active || self.exhausted {
            return;
        }
        let Some(revision) = self.revision.checked_add(1) else {
            self.exhausted = true;
            self.pending = None;
            return;
        };
        self.revision = revision;
        self.pending = Some(ClipboardChange {
            revision,
            owner,
            time,
            origin: None,
        });
    }
    pub(super) fn stop(&mut self) {
        self.active = false;
        self.pending = None;
    }
}
impl X11Clipboard {
    /// Subscribe only after observation admission. The initial current selection
    /// is a fresh native observation; queued events from a previous subscription
    /// cannot re-enter it. No payload is read, and no thread/runtime is started.
    /// Requires `XFixes` version 1; there is no silent content-polling fallback.
    pub fn start_watching(&mut self) -> Result<(), WatchError> {
        if self.changes.active {
            return Err(WatchError::AlreadyWatching);
        }
        if self.changes.exhausted {
            return Err(WatchError::GenerationExhausted);
        }
        // SAFETY: uniquely accessed live connection; the native boundary owns
        // the observer child and negotiates fixed-size XFixes version-1 records.
        match unsafe { ffi::fr_clip_subscribe(self.handle()?) } {
            0 => {}
            -3 => return Err(WatchError::Unsupported),
            _ => return Err(PlatformError::Unavailable.into()),
        }
        self.changes.active = true;
        let mut owner = 0;
        // SAFETY: writable scalar output; subscription is established first so
        // owner changes during bootstrap remain queued, rather than being lost.
        if unsafe { ffi::fr_clip_owner(self.handle()?, &raw mut owner) } != 0 {
            self.close();
            return Err(PlatformError::Unavailable.into());
        }
        let time = if owner == self.window {
            self.current.as_ref().map_or(0, |selection| selection.time)
        } else {
            0
        };
        self.changes.record(owner, time);
        Ok(())
    }
    /// Remove only this subscription. A fresh observer window is used on the
    /// next enable; outstanding native reads remain the caller's responsibility.
    pub fn stop_watching(&mut self) -> Result<(), WatchError> {
        self.changes.stop();
        // SAFETY: C consumes only its private observer child, never an OS
        // clipboard owner. A disconnect failure makes this whole owner unusable.
        if unsafe { ffi::fr_clip_unsubscribe(self.handle()?) } != 0 {
            self.close();
            return Err(PlatformError::Unavailable.into());
        }
        Ok(())
    }
    /// Service at most 32 events and return only the newest known change after
    /// the event queue has caught up. A full turn yields None without losing the
    /// pending revision; continuous floods cannot force unbounded draining.
    /// This never requests text. Use the revision to fence an in-flight read.
    pub fn poll_change(&mut self) -> Result<Option<ClipboardChange>, WatchError> {
        let (change, settled) = self.poll_change_turn()?;
        Ok(if settled { change } else { None })
    }
    pub(super) fn poll_change_turn(
        &mut self,
    ) -> Result<(Option<ClipboardChange>, bool), WatchError> {
        if !self.changes.active {
            return Err(WatchError::NotWatching);
        }
        let count = self.pump()?;
        if self.changes.exhausted {
            return Err(WatchError::GenerationExhausted);
        }
        let settled = count < 32;
        let pending = if settled {
            self.changes.pending.take()
        } else {
            self.changes.pending
        };
        let Some(mut change) = pending else {
            return Ok((None, settled));
        };
        // Exact publication provenance, not equality of text bytes. An old
        // notification from an earlier own publication is never mislabeled.
        change.origin = self.current.as_ref().and_then(|selection| {
            (change.owner == self.window && change.time == selection.time)
                .then_some(selection.stamp)
        });
        Ok((Some(change), settled))
    }
    /// Latest serviced revision, including a change still being coalesced.
    /// Valid only for this connection; it does not itself authorize anything.
    pub const fn change_revision(&self) -> u64 {
        self.changes.revision
    }
}
