//! Original macOS shortcut-capture custody. The native tap only receives an
//! atomic stop fence; it never borrows an input source or its authority locks.
//! A requested stop and observed native callback retirement remain separate.
#![forbid(unsafe_code)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

/// Convert the original capture bound conservatively. The caller samples
/// native uptime BEFORE sampling the input owner's clock: time spent between
/// those reads reduces permission instead of extending it. Native admission
/// independently caps this absolute bound to its short pump watchdog.
pub(crate) fn native_deadline(
    native_before: Option<Duration>,
    client_now_us: Option<u64>,
    client_until_us: u64,
) -> Option<Duration> {
    let remaining = client_until_us.checked_sub(client_now_us?)?;
    if remaining == 0 {
        return None;
    }
    native_before?.checked_add(Duration::from_micros(remaining))
}

/// A queued key retains the context and client deadline that were published
/// before its native callback. Later ticket renewal cannot extend either.
pub(crate) fn captured_deadline(
    captured_context: u32,
    current_context: u32,
    captured_until: u64,
    sampled: u64,
    now: u64,
    max_age: u64,
) -> Option<u64> {
    if captured_context == 0 || captured_context != current_context || sampled > now {
        return None;
    }
    let until = captured_until.min(sampled.checked_add(max_age)?);
    (now < until).then_some(until)
}

/// Actual retirement of one original native callback attachment. Stopping the
/// source alone cannot acknowledge removal of its tap or release its callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    DisabledByUser,
    TapDisabled,
    Expired,
    NativeFailure,
}

#[derive(Clone)]
pub(crate) struct Retirement {
    complete: Arc<AtomicBool>,
    reason: Arc<AtomicU8>,
}
impl Retirement {
    pub(crate) fn is_complete(&self) -> bool {
        self.complete.load(Ordering::SeqCst)
    }

    pub(crate) fn reason(&self) -> Option<Stop> {
        match self.reason.load(Ordering::SeqCst) {
            0 => None,
            1 => Some(Stop::DisabledByUser),
            2 => Some(Stop::TapDisabled),
            3 => Some(Stop::Expired),
            _ => Some(Stop::NativeFailure),
        }
    }
}

pub(crate) struct Owner {
    stop: Arc<AtomicBool>,
    retirement: Retirement,
}
impl Owner {
    pub(crate) fn new(stop: Arc<AtomicBool>) -> (Self, Retirement) {
        let retirement = Retirement {
            complete: Arc::new(AtomicBool::new(false)),
            reason: Arc::new(AtomicU8::new(0)),
        };
        (
            Self {
                stop,
                retirement: retirement.clone(),
            },
            retirement,
        )
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn native_stop(&self, reason: u32) {
        // Record only an actual native cause. Routine source closure and
        // requested native cleanup have reason zero and cannot invent failure.
        let reason = match reason {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            _ => 4,
        };
        if reason != 0 {
            let _ = self.retirement.reason.compare_exchange(
                0,
                reason,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        self.stop();
    }

    /// Only after native tap invalidation, source removal and callback release.
    pub(crate) fn complete(&self) {
        self.stop();
        self.retirement.complete.store(true, Ordering::SeqCst);
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // An unexpected Rust unwind is a fence, never cleanup evidence.
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_keys_keep_their_original_context_and_ticket_deadline() {
        assert_eq!(captured_deadline(7, 7, 120, 90, 100, 50), Some(120));
        assert_eq!(captured_deadline(7, 7, 120, 90, 120, 50), None);
        assert_eq!(captured_deadline(7, 8, 120, 90, 100, 50), None);
        assert_eq!(captured_deadline(0, 0, 120, 90, 100, 50), None);
        // A still-live native snapshot does not extend the original event age.
        assert_eq!(captured_deadline(7, 7, 500, 90, 100, 50), Some(140));
        assert_eq!(captured_deadline(7, 7, 500, 90, 140, 50), None);
    }

    #[test]
    fn malformed_or_overflowed_capture_times_never_authorize_a_key() {
        assert_eq!(captured_deadline(1, 1, 0, 10, 10, 50), None);
        assert_eq!(captured_deadline(1, 1, 100, 11, 10, 50), None);
        assert_eq!(
            captured_deadline(1, 1, u64::MAX, u64::MAX - 1, u64::MAX - 1, 50),
            None
        );
    }

    #[test]
    fn earlier_native_sample_cannot_extend_the_original_capture_bound() {
        assert_eq!(
            native_deadline(Some(Duration::from_secs(10)), Some(8_000), 8_750),
            Some(Duration::from_micros(10_000_750)),
        );
        assert_eq!(
            native_deadline(Some(Duration::from_secs(10)), Some(8_600), 8_750),
            Some(Duration::from_micros(10_000_150)),
        );
    }

    #[test]
    fn expired_or_unavailable_original_clocks_never_arm_the_native_tap() {
        let native = Some(Duration::from_secs(10));
        for until in [0, 7_999, 8_000] {
            assert_eq!(native_deadline(native, Some(8_000), until), None);
        }
        assert_eq!(native_deadline(None, Some(8_000), 9_000), None);
        assert_eq!(native_deadline(native, None, 9_000), None);
        assert_eq!(native_deadline(Some(Duration::MAX), Some(0), 1), None);
    }

    #[test]
    fn source_stop_does_not_acknowledge_native_callback_retirement() {
        let stop = Arc::new(AtomicBool::new(false));
        let (owner, retirement) = Owner::new(Arc::clone(&stop));
        assert!(!owner.stopped());
        assert!(!retirement.is_complete());
        stop.store(true, Ordering::SeqCst);
        assert!(owner.stopped());
        assert!(!retirement.is_complete());
        owner.complete();
        assert!(retirement.is_complete());
    }

    #[test]
    fn old_callback_can_only_stop_and_retire_its_original_source() {
        let old_stop = Arc::new(AtomicBool::new(false));
        let new_stop = Arc::new(AtomicBool::new(false));
        let (old, old_retirement) = Owner::new(Arc::clone(&old_stop));
        let (new, new_retirement) = Owner::new(Arc::clone(&new_stop));
        old.complete();
        assert!(old_stop.load(Ordering::SeqCst));
        assert!(old_retirement.is_complete());
        assert!(!new.stopped());
        assert!(!new_retirement.is_complete());
        drop(new);
        assert!(new_stop.load(Ordering::SeqCst));
        assert!(!new_retirement.is_complete());
    }

    #[test]
    fn native_stop_retains_its_first_cause_and_cleanup_cannot_invent_one() {
        let (owner, retirement) = Owner::new(Arc::new(AtomicBool::new(false)));
        owner.native_stop(1);
        owner.native_stop(2);
        owner.complete();
        assert_eq!(retirement.reason(), Some(Stop::DisabledByUser));
        let (owner, retirement) = Owner::new(Arc::new(AtomicBool::new(false)));
        owner.native_stop(0);
        owner.complete();
        assert_eq!(retirement.reason(), None);
    }
}
