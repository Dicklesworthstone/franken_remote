//! Finite observation policy for the original Mac viewer and cleanup owner.
//! Portable tests cover receipt ownership and closure, not `AppKit` or the GPU.
#![forbid(unsafe_code)]

use fr_client::robot::live::Evidence;
use fr_media::worker::presentation_snapshot::Snapshot;
use fr_wire::{
    closure::Reason,
    display::Display,
    negotiation::{Offer, Role},
};
use std::{ops::ControlFlow, time::Instant};

/// The original native receipt retained through a successful finite operation
/// and positively completed cleanup. This is historical observation, never a
/// live input handle or evidence of source freshness or physical visibility.
/// Pixel contents are deliberately absent from `Debug` and cannot be cloned.
pub struct InitialObservation {
    pub display: Display,
    pub serial: u64,
    pub evidence: Evidence,
    pub received_at: Instant,
    pub snapshot: Option<Snapshot>,
}

enum State {
    Pending,
    Running,
    Ready(InitialObservation),
    Taken,
}

pub(crate) struct Once {
    screenshot: bool,
    state: State,
}

impl Once {
    pub(crate) const fn new(screenshot: bool) -> Self {
        Self {
            screenshot,
            state: State::Pending,
        }
    }

    pub(crate) const fn accepts_attempt(&self, attempt: u8) -> bool {
        attempt == 1 && matches!(self.state, State::Pending)
    }

    pub(crate) fn begin(&mut self, attempt: u8) -> Result<(), ()> {
        if !self.accepts_attempt(attempt) {
            return Err(());
        }
        self.state = State::Running;
        Ok(())
    }

    pub(crate) const fn screenshot(&self) -> bool {
        self.screenshot
    }

    pub(crate) fn retain(&mut self, original: InitialObservation) -> Result<(), ()> {
        if !matches!(self.state, State::Running)
            || original.snapshot.as_ref().is_some_and(|image| {
                !self.screenshot
                    || image.stamp().frame.as_raw() != original.serial
                    || original.evidence != Evidence::SubmittedToCompositor
            })
        {
            return Err(());
        }
        self.state = State::Ready(original);
        Ok(())
    }

    /// A request for pixels which yielded routine absence still retains its
    /// actual receipt. The caller can report absence after ordinary cleanup.
    pub(crate) fn take(
        &mut self,
        operation_succeeded: bool,
        cleanup_confirmed: bool,
    ) -> Option<InitialObservation> {
        if !operation_succeeded || !cleanup_confirmed || !matches!(self.state, State::Ready(_)) {
            return None;
        }
        match std::mem::replace(&mut self.state, State::Taken) {
            State::Ready(original) => Some(original),
            _ => unreachable!("the same owner held its ready receipt"),
        }
    }

    pub(crate) fn discard(&mut self) {
        self.state = State::Taken;
    }
}

/// A finite observation may only remove optional capabilities from the
/// original observation profile. No auxiliary lane or control is requested.
pub(crate) fn offer(original: &Offer) -> Result<Offer, ()> {
    if original.role != Role::Observe {
        return Err(());
    }
    let baseline = fr_client::native::observation_offer();
    let keep = |name: &str| baseline.capabilities.iter().any(|cap| cap.name == name);
    if original
        .capabilities
        .iter()
        .any(|cap| cap.required && !keep(&cap.name))
    {
        return Err(());
    }
    let mut finite = original.clone();
    finite.capabilities.retain(|cap| keep(&cap.name));
    Ok(finite)
}

pub(crate) fn flow(
    once: bool,
    continuous: impl FnOnce() -> Result<ControlFlow<Reason>, ()>,
) -> Result<ControlFlow<Reason>, ()> {
    if once {
        // The initial receipt already exists. An idle desktop need not produce
        // another frame to close this original observation and enter cleanup.
        Ok(ControlFlow::Break(Reason::InspectionComplete))
    } else {
        continuous()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration,
    };
    use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
    use std::{cell::Cell, time::Duration};

    fn receipt(serial: u64, image: bool, at: Instant) -> InitialObservation {
        InitialObservation {
            display: Display {
                handle: 17,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: -320,
                y: 0,
                pixel_width: 320,
                pixel_height: 240,
                logical_width: 160,
                logical_height: 120,
                scale_numerator: 2,
                scale_denominator: 1,
                rotation: 0,
            },
            serial,
            evidence: Evidence::SubmittedToCompositor,
            received_at: at,
            snapshot: image.then(|| {
                let stamp = Stamp {
                    frame: FrameId::from_raw(serial),
                    configuration: CodecConfigurationGeneration::from_raw(3),
                    recovery: RecoveryGeneration::from_raw(7),
                };
                Snapshot::decode(Snapshot::encode(stamp, 2, 1, &[0x7f; 8]).unwrap()).unwrap()
            }),
        }
    }

    #[test]
    fn finite_observation_consumes_only_the_original_attempt() {
        let mut once = Once::new(false);
        assert!(!once.screenshot());
        assert!(once.accepts_attempt(1));
        assert!(once.retain(receipt(17, false, Instant::now())).is_err());
        for attempt in [0, 2, 32, u8::MAX] {
            assert!(once.begin(attempt).is_err());
        }
        once.begin(1).unwrap();
        assert!(!once.accepts_attempt(1));
        assert!(once.begin(1).is_err());
        once.discard();
        assert!(once.begin(1).is_err());
        assert!(once.take(true, true).is_none());
    }

    #[test]
    fn original_receipt_and_image_wait_for_successful_operation_and_actual_cleanup() {
        let mut once = Once::new(true);
        let at = Instant::now();
        let original = receipt(17, true, at);
        let pixels = original.snapshot.as_ref().unwrap().pixels().as_ptr();
        once.begin(1).unwrap();
        once.retain(original).unwrap();
        assert!(
            once.retain(receipt(18, true, at + Duration::from_secs(1)))
                .is_err()
        );
        for (success, cleanup) in [(false, false), (true, false), (false, true)] {
            assert!(once.take(success, cleanup).is_none());
        }
        let original = once.take(true, true).unwrap();
        assert_eq!(original.display.handle, 17);
        assert_eq!(original.serial, 17);
        assert_eq!(original.received_at, at);
        assert_eq!(original.snapshot.unwrap().pixels().as_ptr(), pixels);
        assert!(once.take(true, true).is_none());
        assert!(once.retain(receipt(19, true, at)).is_err());
        assert!(once.begin(1).is_err());
    }

    #[test]
    fn routine_snapshot_absence_keeps_a_historical_receipt_and_no_pixels() {
        for screenshot in [false, true] {
            let mut once = Once::new(screenshot);
            let at = Instant::now();
            once.begin(1).unwrap();
            let mut original = receipt(17, false, at);
            original.evidence = Evidence::Decoded;
            once.retain(original).unwrap();
            let original = once.take(true, true).unwrap();
            assert_eq!(original.received_at, at);
            assert_eq!(original.evidence, Evidence::Decoded);
            assert!(original.snapshot.is_none());
        }
    }

    #[test]
    fn unexpected_pixels_wrong_frame_and_decode_only_evidence_cannot_be_retained() {
        let at = Instant::now();
        for case in 0..3 {
            let mut once = Once::new(case != 0);
            once.begin(1).unwrap();
            let mut original = receipt(17, true, at);
            if case == 1 {
                original.serial = 18;
            } else if case == 2 {
                original.evidence = Evidence::Decoded;
            }
            assert!(once.retain(original).is_err());
            assert!(once.take(true, true).is_none());
        }
    }

    #[test]
    fn a_failed_original_service_discards_its_retained_artifact_permanently() {
        let mut once = Once::new(true);
        once.begin(1).unwrap();
        once.retain(receipt(17, true, Instant::now())).unwrap();
        once.discard();
        assert!(once.take(true, true).is_none());
        assert!(once.begin(1).is_err());
    }

    #[test]
    fn finite_offer_does_not_request_control_or_auxiliary_lanes() {
        let original = fr_client::native::observation_offer();
        assert_eq!(offer(&original).unwrap(), original);
        assert!(offer(&fr_client::native::control_offer()).is_err());
        let mut audio = fr_client::native::observation_offer_with_audio();
        assert_eq!(offer(&audio).unwrap(), original);
        audio
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == fr_wire::audio::CAPABILITY)
            .unwrap()
            .required = true;
        assert!(offer(&audio).is_err());
    }

    #[test]
    fn finite_service_closes_without_waiting_for_a_second_picture() {
        let called = Cell::new(false);
        assert_eq!(
            flow(true, || {
                called.set(true);
                Err(())
            }),
            Ok(ControlFlow::Break(Reason::InspectionComplete))
        );
        assert!(!called.get());
        assert_eq!(
            flow(false, || {
                called.set(true);
                Ok(ControlFlow::Continue(()))
            }),
            Ok(ControlFlow::Continue(()))
        );
        assert!(called.get());
        assert!(flow(false, || Err(())).is_err());
    }
}
