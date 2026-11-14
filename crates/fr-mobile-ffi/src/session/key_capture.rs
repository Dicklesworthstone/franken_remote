//! One original input source and exclusive deadline for a native event.
//! Shared by keys, absolute motion, buttons and wheel input. No event content,
//! OS calls or authority are stored in this token.
#![forbid(unsafe_code)]

use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(super) struct Capture {
    source: Weak<AtomicBool>,
    opened: u64,
    sampled: u64,
    until: u64,
}

impl Capture {
    pub(super) fn new(
        source: &Arc<AtomicBool>,
        opened: u64,
        sampled: u64,
        max_age: u64,
    ) -> Option<Self> {
        if sampled < opened {
            return None;
        }
        Some(Self {
            source: Arc::downgrade(source),
            opened,
            sampled,
            until: sampled.checked_add(max_age)?,
        })
    }

    /// Sample the client clock BEFORE measuring native queue age. Subtracting
    /// that whole age then also charges any lock wait or interrupted sample.
    pub(super) fn with_queue_age(mut self, age: Duration) -> Option<Self> {
        let age = u64::try_from(age.as_nanos().div_ceil(1_000)).ok()?;
        // An event created before this source attached cannot become input for
        // the new grant merely because Windows dispatches it afterward.
        if self.sampled.checked_sub(age)? < self.opened {
            return None;
        }
        self.until = self.until.checked_sub(age)?;
        (self.until > self.sampled).then_some(self)
    }

    pub(super) fn current(&self, source: &Arc<AtomicBool>, now: u64) -> bool {
        self.source.ptr_eq(&Arc::downgrade(source))
            && !source.load(Ordering::SeqCst)
            && self.sampled <= now
            && now < self.until
    }

    pub(super) const fn bounds(&self) -> (u64, u64) {
        (self.sampled, self.until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_age_shortens_the_original_bound_without_restamping_capture() {
        let source = Arc::new(AtomicBool::new(false));
        let captured = Capture::new(&source, 900_000, 1_000_000, 100_000)
            .unwrap()
            .with_queue_age(Duration::from_micros(40_001))
            .unwrap();
        assert_eq!(captured.bounds(), (1_000_000, 1_059_999));
        assert!(captured.current(&source, 1_059_998));
        assert!(!captured.current(&source, 1_059_999));
        assert!(!captured.current(&source, 1_100_000));
    }

    #[test]
    fn a_new_grant_clock_regression_or_revoked_source_cannot_reuse_capture() {
        let original = Arc::new(AtomicBool::new(false));
        let captured = Capture::new(&original, 900, 1_000, 100).unwrap();
        assert!(!captured.current(&original, 999));
        let replacement = Arc::new(AtomicBool::new(false));
        assert!(!captured.current(&replacement, 1_001));
        original.store(true, Ordering::SeqCst);
        assert!(!captured.current(&original, 1_001));
    }

    #[test]
    fn quantization_and_arithmetic_never_extend_expired_captures() {
        let source = Arc::new(AtomicBool::new(false));
        assert!(Capture::new(&source, 0, u64::MAX, 1).is_none());
        assert!(
            Capture::new(&source, 900, 1_000, 100)
                .unwrap()
                .with_queue_age(Duration::from_micros(100))
                .is_none()
        );
        assert!(
            Capture::new(&source, 900, 1_000, 100)
                .unwrap()
                .with_queue_age(Duration::MAX)
                .is_none()
        );
        let captured = Capture::new(&source, 900, 1_000, 100)
            .unwrap()
            .with_queue_age(Duration::from_nanos(1))
            .unwrap();
        assert_eq!(captured.bounds(), (1_000, 1_099));
    }

    #[test]
    fn an_event_from_before_grant_attachment_never_belongs_to_the_new_source() {
        let source = Arc::new(AtomicBool::new(false));
        assert!(Capture::new(&source, 1_000, 999, 100).is_none());
        assert!(
            Capture::new(&source, 1_000, 1_040, 100)
                .unwrap()
                .with_queue_age(Duration::from_micros(41))
                .is_none()
        );
        let captured = Capture::new(&source, 1_000, 1_040, 100)
            .unwrap()
            .with_queue_age(Duration::from_micros(40))
            .unwrap();
        assert_eq!(captured.bounds(), (1_040, 1_100));
        assert!(captured.current(&source, 1_041));
    }

    #[test]
    fn coordinate_conversion_or_key_drain_cannot_refresh_a_native_action() {
        let original = Arc::new(AtomicBool::new(false));
        let captured = Capture::new(&original, 800_000, 1_000_000, 100_000)
            .unwrap()
            .with_queue_age(Duration::from_millis(60))
            .unwrap();
        // The original native mouse message already spent 60 ms queued.
        // Coordinate conversion and draining earlier keys consume another
        // 40 ms; checking the current source cannot renew those 100 ms.
        assert!(captured.current(&original, 1_039_999));
        assert!(!captured.current(&original, 1_040_000));
        assert_eq!(captured.bounds(), (1_000_000, 1_040_000));
    }

    #[test]
    fn retained_native_actions_never_attach_to_a_recreated_grant() {
        let original = Arc::new(AtomicBool::new(false));
        let captured = Capture::new(&original, 900, 1_000, 100).unwrap();
        drop(original);
        // The Weak keeps the old allocation identity distinct even when a
        // replacement grant has exactly the same numerical clock values.
        for _ in 0..16 {
            let replacement = Arc::new(AtomicBool::new(false));
            assert!(!captured.current(&replacement, 1_001));
        }
    }
}
