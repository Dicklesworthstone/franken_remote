//! Content-free capture diagnostics; `ViewerControl` remains the input authority.
//! Reserving a first cause cannot publish a fence that has not happened yet.
use super::StopReason;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Default)]
pub(super) struct CaptureState {
    published: AtomicU8,
    first_cause: AtomicU8,
}
impl CaptureState {
    pub(super) fn load(&self) -> u8 {
        self.published.load(Ordering::Acquire)
    }
    /// Reserved diagnostic only; callers must establish the real input fence
    /// before using this as a terminal attachment error.
    pub(super) fn first_stop(&self) -> Option<StopReason> {
        match super::state(self.first_cause.load(Ordering::Acquire)) {
            super::Status::Stopped(reason) => Some(reason),
            _ => None,
        }
    }
    pub(super) fn ready(&self) {
        let _ = self
            .published
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(super) fn stop(&self, reason: StopReason, fence: impl FnOnce()) {
        // The real fence can immediately wake cleanup on another thread. Its
        // local stop must not replace the native failure that woke it.
        let cause = match self.first_cause.compare_exchange(
            0,
            reason as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => reason as u8,
            Err(first) => first,
        };
        fence();
        let _ = self
            .published
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old < 2).then_some(cause)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cleanup_woken_by_the_fence_cannot_replace_the_original_cause() {
        for running in [false, true] {
            for reason in [
                StopReason::SystemShortcutsUnavailable,
                StopReason::LocalEscape,
            ] {
                let capture = CaptureState::default();
                if running {
                    capture.ready();
                }
                let fenced = Cell::new(false);
                capture.stop(reason, || {
                    // A reserved diagnostic is not yet a published input fence.
                    assert_eq!(capture.load(), u8::from(running));
                    fenced.set(true);
                    // Deterministic reentrant wake: production ViewerControl
                    // fences then wakes Session cleanup, which calls local stop.
                    capture.stop(StopReason::Local, || assert!(fenced.get()));
                    assert_eq!(capture.load(), reason as u8);
                });
                assert!(fenced.get());
                capture.ready();
                capture.stop(StopReason::NativeFailure, || assert!(fenced.get()));
                assert_eq!(capture.load(), reason as u8);
            }
        }
    }
}
