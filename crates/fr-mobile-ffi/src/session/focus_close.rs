//! One original input grant's irreversible local focus-close claim.
//! The caller serializes publication/retirement with attempt cleanup and
//! separately checks current visibility, serving state and native ownership.
#![forbid(unsafe_code)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct Original {
    pub attempt: u64,
    pub stop: Arc<AtomicBool>,
}

impl Original {
    /// Claim close before stopping the producer, so queued native callbacks
    /// retain the source for orderly cleanup. A retired original can never
    /// claim a later attempt. Failure never creates a cleanup proof.
    pub fn claim(&self, requested: u64, current: u64, close: &AtomicBool) -> bool {
        if requested != self.attempt
            || current != self.attempt
            || self.stop.load(Ordering::SeqCst)
            || close
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        // A source may fail while this request claims close. Keep that close
        // irreversible, but refuse the request if its original already stopped.
        self.stop
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_claim_is_one_use_and_irreversibly_fences_its_original() {
        let stop = Arc::new(AtomicBool::new(false));
        let close = AtomicBool::new(false);
        let original = Original { attempt: 7, stop };
        assert!(original.claim(7, 7, &close));
        assert!(close.load(Ordering::SeqCst));
        assert!(original.stop.load(Ordering::SeqCst));
        assert!(!original.claim(7, 7, &close));
    }

    #[test]
    fn old_attempt_or_callback_cannot_close_a_later_owner() {
        for (requested, current) in [(6, 7), (7, 8), (8, 8), (7, 6)] {
            let original = Original {
                attempt: 7,
                stop: Arc::new(AtomicBool::new(false)),
            };
            let close = AtomicBool::new(false);
            assert!(!original.claim(requested, current, &close));
            assert!(!close.load(Ordering::SeqCst));
            assert!(!original.stop.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn a_retired_grant_or_prior_explicit_close_cannot_become_focus_success() {
        for (stopped, closed) in [(true, false), (false, true), (true, true)] {
            let original = Original {
                attempt: 7,
                stop: Arc::new(AtomicBool::new(stopped)),
            };
            let close = AtomicBool::new(closed);
            assert!(!original.claim(7, 7, &close));
            assert_eq!(close.load(Ordering::SeqCst), closed);
            assert_eq!(original.stop.load(Ordering::SeqCst), stopped);
        }
    }
}
