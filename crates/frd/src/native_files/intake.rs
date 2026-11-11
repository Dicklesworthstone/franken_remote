//! One bounded local selection slot on an already negotiated upload lane.
//! Descriptors are selected by the UI off the session thread; this owner never
//! opens a path, reads a source, grants control, or retries an earlier transfer.
use super::{Absence, SendControl, SendRequest};
use asupersync::cx::Cx;
use fr_files::sender::{Error, Policy};
use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

pub(crate) struct Pending {
    pub(crate) request: SendRequest,
    pub(crate) control: SendControl,
    pub(crate) until_us: u64,
}
struct State {
    pending: Option<Pending>,
    ready: bool,
    closed: bool,
    last: u64,
}
struct Shared {
    cx: Cx,
    policy: Policy,
    state: Mutex<State>,
}
/// An explicit next selection for ONE original desktop/file attachment. At most
/// one bounded selection can wait. Every accepted selection gets a fresh result
/// handle; a terminal handle from an earlier batch cannot affect its successor.
#[derive(Clone)]
pub struct SendIntake(Arc<Shared>);
impl fmt::Debug for SendIntake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSendIntake")
            .field("ready", &self.ready())
            .field("closed", &self.is_closed())
            .finish()
    }
}
impl SendIntake {
    pub(crate) fn new(cx: Cx, policy: Policy) -> Self {
        Self(Arc::new(Shared {
            cx,
            policy,
            state: Mutex::new(State {
                pending: None,
                ready: false,
                closed: false,
                last: 0,
            }),
        }))
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn now(&self) -> Result<u64, Error> {
        self.0.cx.checkpoint().map_err(|_| Error::Closed)?;
        Ok(self
            .0
            .cx
            .timer_driver()
            .ok_or(Error::Clock)?
            .now()
            .as_nanos()
            / 1000)
    }
    /// Local selection capacity, not an authority or publication guarantee.
    /// The original controller still rechecks permission and view freshness
    /// before handing a selection to its original bounded disk sender.
    pub fn ready(&self) -> bool {
        if self.0.cx.is_cancel_requested() {
            return false;
        }
        let state = self.lock();
        state.ready && !state.closed && state.pending.is_none()
    }
    pub fn is_closed(&self) -> bool {
        self.0.cx.is_cancel_requested() || self.lock().closed
    }
    /// Accept exactly one newly selected batch. Its absolute lifetime begins
    /// here, including time awaiting an ordinary controller turn. The immutable
    /// sender policy cannot be changed by another request, and a full slot is a
    /// refusal, never a replacement of selected descriptors or publication data.
    pub fn submit(&self, request: SendRequest) -> Result<SendControl, Error> {
        self.submit_with_clock(request, || self.now())
    }
    fn submit_with_clock(
        &self,
        request: SendRequest,
        mut clock: impl FnMut() -> Result<u64, Error>,
    ) -> Result<SendControl, Error> {
        let p = request.policy;
        let original = self.0.policy;
        if p.max_file_bytes != original.max_file_bytes
            || p.bytes_per_second != original.bytes_per_second
            || p.transfer_lifetime != original.transfer_lifetime
            || p.record_lifetime != original.record_lifetime
            || request.selection.is_empty()
            || !(Duration::from_micros(1)..=Duration::from_secs(3600)).contains(&request.lifetime)
        {
            return Err(Error::Limits);
        }
        let submitted = clock()?;
        let until_us = submitted
            .checked_add(u64::try_from(request.lifetime.as_micros()).map_err(|_| Error::Limits)?)
            .ok_or(Error::Clock)?;
        let mut state = self.lock();
        if state.closed || self.0.cx.is_cancel_requested() {
            return Err(Error::Closed);
        }
        // Another owner may advance `last` while this submission waits for the
        // mutex. Order a fresh sample under the SAME lock instead of treating
        // the earlier submission timestamp as a clock regression. Waiting can
        // spend the original lifetime, never restart it at this later sample.
        let now = clock()?;
        if now < submitted || now < state.last {
            return Err(Error::Clock);
        }
        state.last = now;
        if now >= until_us {
            return Err(Error::Expired);
        }
        if !state.ready || state.pending.is_some() {
            return Err(Error::Busy);
        }
        let control = SendControl::new();
        state.ready = false;
        state.pending = Some(Pending {
            request,
            control: control.clone(),
            until_us,
        });
        Ok(control)
    }
    pub(crate) fn set_ready(&self, ready: bool) {
        let mut state = self.lock();
        state.ready = ready && !state.closed && !self.0.cx.is_cancel_requested();
    }
    pub(crate) fn take(&self) -> Option<Pending> {
        self.lock().pending.take()
    }
    /// Retire a queued selection's local cancellation/deadline without waiting
    /// for source admission. A stale view may hold `take` back indefinitely, but
    /// cannot extend this request's original lifetime or retain cancelled files.
    /// The returned request has never entered a sender or started a disk worker.
    pub(crate) fn take_terminal(&self) -> Option<(Pending, Error)> {
        self.take_terminal_with_clock(|| self.now())
    }
    fn take_terminal_with_clock(
        &self,
        mut clock: impl FnMut() -> Result<u64, Error>,
    ) -> Option<(Pending, Error)> {
        let mut state = self.lock();
        let pending = state.pending.as_ref()?;
        let until = pending.until_us;
        let reason = if pending.control.cancel_requested() {
            Some(Error::Cancelled)
        } else {
            match clock() {
                Err(error) => Some(error),
                Ok(now) if now < state.last => Some(Error::Clock),
                Ok(now) => {
                    state.last = now;
                    (now >= until).then_some(Error::Expired)
                }
            }
        }?;
        state.ready = false;
        state.pending.take().map(|pending| (pending, reason))
    }
    pub(crate) fn close(&self) {
        let pending = {
            let mut state = self.lock();
            state.closed = true;
            state.ready = false;
            state.pending.take()
        };
        if let Some(pending) = pending {
            pending.control.end(Absence::Refused(Error::Cancelled));
            pending.control.cleaned(Some(Ok(())));
        }
    }
}

#[cfg(test)]
mod tests;
