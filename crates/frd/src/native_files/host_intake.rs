//! One local command surface for one original host download attachment.
//! Binding creates no file or input authority, and can never be repeated.
use super::{Absence, SendControl, SendIntake, SendRequest};
use asupersync::cx::Cx;
use fr_files::sender::{Error, Policy};
use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Default)]
struct State {
    intake: Option<SendIntake>,
    latest: Option<SendControl>,
    closed: bool,
}

/// Explicit local selections for ONE original host session. Before attachment
/// this handle refuses submission rather than queueing for a future controller.
/// A stale command can never bind this surface to a replacement session.
#[derive(Clone)]
pub struct HostSendIntake(Arc<Mutex<State>>);
impl fmt::Debug for HostSendIntake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFileSendIntake")
            .field("ready", &self.ready())
            .field("closed", &self.is_closed())
            .finish()
    }
}
impl HostSendIntake {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(State::default())))
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn bind(&self, cx: Cx) -> Result<SendIntake, Absence> {
        let mut state = self.lock();
        if state.closed || state.intake.is_some() {
            return Err(Absence::Unavailable);
        }
        if cx.is_cancel_requested() {
            state.closed = true;
            return Err(Absence::Unavailable);
        }
        let intake = SendIntake::new(cx, Policy::default());
        state.intake = Some(intake.clone());
        Ok(intake)
    }
    /// Capacity only. The original controller still rechecks authority and
    /// freshness before any source starts; a source selection is never a grant.
    pub fn ready(&self) -> bool {
        let state = self.lock();
        !state.closed && state.intake.as_ref().is_some_and(SendIntake::ready)
    }
    pub fn is_closed(&self) -> bool {
        let state = self.lock();
        state.closed || state.intake.as_ref().is_some_and(SendIntake::is_closed)
    }
    /// Admit one already pinned local selection. No source is read here. The
    /// original absolute lifetime starts at this handoff, and each admitted
    /// selection has its own retained progress, receipts and cancellation.
    pub fn submit(&self, request: SendRequest) -> Result<SendControl, Error> {
        let mut state = self.lock();
        if state.closed {
            return Err(Error::Closed);
        }
        let control = state.intake.as_ref().ok_or(Error::Busy)?.submit(request)?;
        state.latest = Some(control.clone());
        Ok(control)
    }
    /// Terminal for every clone, including a local selection worker that has
    /// not returned yet. Retire the pending slot and request cancellation of
    /// the current batch; neither erases publication or proves worker cleanup.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        if let Some(intake) = &state.intake {
            intake.close();
        }
        if let Some(control) = &state.latest {
            control.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
