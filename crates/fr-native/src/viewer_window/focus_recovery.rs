//! One original window's bounded, focus-only retirement receipt.
//!
//! Input requests verification before fencing its session. Only the original
//! native window thread can confirm it; an explicit local stop always wins.
use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) const MAX_AGE: Duration = Duration::from_millis(100);
const DISABLED: u8 = 0;
const ARMED: u8 = 1;
const REQUESTING: u8 = 2;
const PENDING: u8 = 3;
const VERIFIED: u8 = 4;
const CANCELLED: u8 = 5;

#[derive(Default)]
pub(super) struct Gate {
    state: AtomicU8,
    requested: OnceLock<Instant>,
}
impl Gate {
    pub(super) fn enable(&self) -> bool {
        self.state
            .compare_exchange(DISABLED, ARMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn enabled(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARMED
    }

    pub(super) fn request(&self, now: Instant) -> bool {
        if self
            .state
            .compare_exchange(ARMED, REQUESTING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if self.requested.set(now).is_err() {
            self.cancel();
            return false;
        }
        self.state
            .compare_exchange(REQUESTING, PENDING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn pending(&self, now: Instant) -> bool {
        self.state.load(Ordering::Acquire) == PENDING
            && self.requested.get().is_some_and(|requested| {
                now.checked_duration_since(*requested)
                    .is_some_and(|age| age < MAX_AGE)
            })
    }

    pub(super) fn seal(&self, now: Instant) -> bool {
        if !self.pending(now) {
            self.cancel();
            return false;
        }
        self.state
            .compare_exchange(PENDING, VERIFIED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Historical native evidence only. The enclosing attempt consumes its
    /// exact window owner once; this read permits rechecking a racing veto.
    pub(super) fn verified(&self) -> bool {
        self.state.load(Ordering::Acquire) == VERIFIED
    }

    pub(super) fn cancel(&self) {
        self.state.store(CANCELLED, Ordering::Release);
    }

    /// Initial mapping and placement can happen before the desktop opts in.
    /// Once armed, uncertainty permanently invalidates this original owner.
    pub(super) fn invalidate(&self) {
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state != DISABLED).then_some(CANCELLED)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_armed_request_can_be_verified() {
        let gate = Gate::default();
        let now = Instant::now();
        assert!(!gate.enabled());
        assert!(!gate.request(now));
        gate.invalidate();
        assert!(gate.enable());
        assert!(gate.enabled());
        assert!(!gate.enable());
        assert!(gate.request(now));
        assert!(!gate.enabled());
        assert!(!gate.request(now));
        assert!(!gate.verified());
        assert!(gate.seal(now + MAX_AGE / 2));
        assert!(gate.verified());
        assert!(!gate.request(now));
        gate.cancel();
        assert!(!gate.enabled());
        assert!(!gate.verified());
        assert!(!gate.enable());
    }

    #[test]
    fn expired_cancelled_and_unrequested_receipts_fail_closed() {
        for elapsed in [MAX_AGE, MAX_AGE * 2] {
            let gate = Gate::default();
            let now = Instant::now();
            assert!(gate.enable());
            assert!(gate.request(now));
            assert!(!gate.seal(now + elapsed));
            assert!(!gate.verified());
        }
        for requested in [false, true] {
            let gate = Gate::default();
            let now = Instant::now();
            assert!(gate.enable());
            if requested {
                assert!(gate.request(now));
            }
            gate.cancel();
            assert!(!gate.request(now));
            assert!(!gate.seal(now));
            assert!(!gate.verified());
        }
    }
}
