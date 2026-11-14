//! One explicit control choice belongs to one ready, visible attempt.
//! The token is comparison metadata, never host input authority. Retirement
//! destroys the choice even when the next attempt shows the same desktop.
#![forbid(unsafe_code)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Native menu input may wait this long before the original request starts.
pub const MAX_GESTURE_AGE: Duration = Duration::from_millis(250);

/// An original, currently offered local control choice. Cloning does not
/// duplicate the one permitted submission or extend its deadline.
#[derive(Clone)]
pub struct Request(Arc<()>);

/// What the session currently knows about control, independently of pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// This connection was explicitly opened for observation only.
    Unavailable,
    /// Waiting for a current visible view before offering a local request.
    WaitingForView,
    /// Viewing, with a current local request available.
    Viewing,
    /// Requested locally or from the host; no input grant is implied.
    Requesting,
    /// The original host grant has an attached input producer.
    Controlled,
    Closing,
    Ended,
}

/// The legacy FFI request may precede the first visible frame, but its call
/// must not wait across retirement and attach to another attempt. Capture
/// before taking the request mutex; installation and clearing use that mutex.
#[derive(Default)]
pub(super) struct Retirement(AtomicU64);

impl Retirement {
    pub(super) fn capture(&self) -> Option<u64> {
        let epoch = self.0.load(Ordering::SeqCst);
        (epoch != u64::MAX).then_some(epoch)
    }

    pub(super) fn current(&self, epoch: u64) -> bool {
        epoch != u64::MAX && self.0.load(Ordering::SeqCst) == epoch
    }

    /// Exhaustion disables legacy requests permanently; it never wraps an
    /// ancient request back into the current attempt's identity.
    pub(super) fn retire(&self) {
        let _ = self
            .0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            });
    }
}

#[derive(Default)]
pub(super) struct Requests {
    scope: Option<Arc<()>>,
    pending: Option<Instant>,
}

impl Requests {
    pub(super) fn ready(&mut self, ready: bool) {
        if !ready {
            self.retire();
        } else if self.scope.is_none() {
            self.scope = Some(Arc::new(()));
        }
    }

    pub(super) fn request(&self) -> Option<Request> {
        if self.pending.is_some() {
            return None;
        }
        self.scope.as_ref().map(|scope| Request(scope.clone()))
    }

    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn submit(&mut self, request: &Request, now: Instant, until: Instant) -> bool {
        if self.pending.is_some()
            || !self
                .scope
                .as_ref()
                .is_some_and(|scope| Arc::ptr_eq(scope, &request.0))
            || now >= until
            || until.duration_since(now) > MAX_GESTURE_AGE
        {
            return false;
        }
        self.pending = Some(until);
        true
    }

    /// Consume once, including expiry. The callback cannot refresh old intent
    /// while waiting for an input lease, native work or network capacity.
    pub(super) fn take(&mut self, now: Instant) -> bool {
        let Some(until) = self.pending.take() else {
            return false;
        };
        self.scope = None;
        now < until
    }

    pub(super) fn retire(&mut self) {
        self.scope = None;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_call_cannot_wait_across_retirement_or_rebind_after_exhaustion() {
        let retirement = Retirement::default();
        let original = retirement.capture().unwrap();
        assert!(retirement.current(original));
        for _ in 0..16 {
            retirement.retire();
            assert!(!retirement.current(original));
            let current = retirement.capture().unwrap();
            assert!(retirement.current(current));
        }
        let exhausted = Retirement(AtomicU64::new(u64::MAX - 1));
        let last = exhausted.capture().unwrap();
        exhausted.retire();
        exhausted.retire();
        assert!(!exhausted.current(last));
        assert!(!exhausted.current(u64::MAX));
        assert!(exhausted.capture().is_none());
    }

    #[test]
    fn only_a_current_ready_choice_can_request_once() {
        let now = Instant::now();
        let mut requests = Requests::default();
        assert!(requests.request().is_none());
        requests.ready(true);
        let choice = requests.request().unwrap();
        assert!(requests.submit(&choice, now, now + MAX_GESTURE_AGE));
        assert!(requests.pending());
        assert!(requests.request().is_none());
        assert!(!requests.submit(&choice.clone(), now, now + MAX_GESTURE_AGE));
        assert!(requests.take(now + Duration::from_millis(249)));
        assert!(!requests.take(now));
        assert!(!requests.submit(&choice, now, now + MAX_GESTURE_AGE));
    }

    #[test]
    fn an_expired_gesture_is_consumed_without_renewal() {
        let now = Instant::now();
        for elapsed in [MAX_GESTURE_AGE, MAX_GESTURE_AGE + Duration::from_nanos(1)] {
            let mut requests = Requests::default();
            requests.ready(true);
            let choice = requests.request().unwrap();
            assert!(requests.submit(&choice, now, now + MAX_GESTURE_AGE));
            assert!(!requests.take(now + elapsed));
            requests.ready(true);
            assert!(!requests.submit(&choice, now, now + MAX_GESTURE_AGE));
            let fresh = requests.request().unwrap();
            assert!(requests.submit(&fresh, now, now + Duration::from_millis(1)));
        }
    }

    #[test]
    fn hide_reconnect_and_readiness_loss_retire_original_choices() {
        let now = Instant::now();
        let mut requests = Requests::default();
        requests.ready(true);
        let old = requests.request().unwrap();
        assert!(requests.submit(&old, now, now + MAX_GESTURE_AGE));
        requests.ready(false);
        assert!(!requests.take(now));
        assert!(requests.request().is_none());
        requests.ready(true);
        assert!(!requests.submit(&old, now, now + MAX_GESTURE_AGE));
        let current = requests.request().unwrap();
        let mut replacement = Requests::default();
        replacement.ready(true);
        assert!(!replacement.submit(&current, now, now + MAX_GESTURE_AGE));
        requests.retire();
        assert!(!requests.submit(&current, now, now + MAX_GESTURE_AGE));
    }

    #[test]
    fn a_readiness_poll_does_not_extend_a_queued_gesture() {
        let now = Instant::now();
        let mut requests = Requests::default();
        requests.ready(true);
        let choice = requests.request().unwrap();
        let until = now + Duration::from_millis(10);
        assert!(requests.submit(&choice, now, until));
        requests.ready(true);
        assert!(!requests.take(until));
    }

    #[test]
    fn expired_and_oversized_bounds_never_enter_the_queue() {
        let now = Instant::now();
        let mut requests = Requests::default();
        requests.ready(true);
        let choice = requests.request().unwrap();
        assert!(!requests.submit(&choice, now, now));
        assert!(!requests.submit(
            &choice,
            now,
            now + MAX_GESTURE_AGE + Duration::from_nanos(1)
        ));
        assert!(!requests.pending());
    }
}
