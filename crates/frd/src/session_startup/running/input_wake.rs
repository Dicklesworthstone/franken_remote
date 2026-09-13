//! One advisory raw-capture check after an actual native submission. Neither a
//! result nor a scheduling hint proves a changed image or a fresh visible view.
use super::Error;
use crate::input_agent::InputReply;
use fr_media::pacing::{Availability, Controller, Mode, ReceiverEvidence};
use fr_wire::input_result::{SequenceSpace, Stage};

/// One native owner has independent, monotonic action and pointer spaces.
/// Consume every completed position, including refusals; never replay an older
/// submitted receipt as new activity. No key, coordinate, text or ticket is kept.
#[derive(Default)]
pub(super) struct Submitted {
    action: Option<u64>,
    pointer: Option<u64>,
}
impl Submitted {
    pub(super) fn take(&mut self, reply: Option<InputReply>) -> bool {
        let Some(InputReply::Record(result)) = reply else {
            return false;
        };
        let last = match result.space {
            SequenceSpace::Action => &mut self.action,
            SequenceSpace::Pointer => &mut self.pointer,
        };
        if last.is_some_and(|n| result.sequence <= n) {
            return false;
        }
        *last = Some(result.sequence);
        result.submitted_operations != 0
            && matches!(result.stage, Stage::SubmittedToOs | Stage::Observed)
    }
}

const LIFETIME_US: u64 = 250_000;
/// New native effects coalesce into one bounded-lifetime hint. No capture is
/// cancelled, reference evicted, or wake budget accumulated while work is busy.
#[derive(Default)]
pub(super) struct Wake {
    at: Option<u64>,
}
impl Wake {
    pub(super) fn note(&mut self, at_us: u64) {
        self.at = Some(at_us);
    }
    pub(super) fn admitted(&mut self) {
        self.at = None;
    }
    pub(super) fn due(
        &mut self,
        now: u64,
        last_capture: Option<u64>,
        in_flight: bool,
        controller: Option<&Controller>,
    ) -> Result<bool, Error> {
        let Some(at) = self.at else { return Ok(false) };
        let elapsed = now.checked_sub(at).ok_or(Error::Clock)?;
        if elapsed >= LIFETIME_US {
            self.at = None;
            return Ok(false);
        }
        let Some((controller, report)) = controller.and_then(|c| c.report().map(|r| (c, r))) else {
            // Fixed pacing has no idle interval to bypass. No delayed opt-in wake.
            self.at = None;
            return Ok(false);
        };
        if report.mode != Mode::Idle {
            self.at = None;
            return Ok(false);
        }
        // Use THIS service turn's measured admission state. The hint grants no
        // extra cache credit or permission to overrun a slow or unknown decoder.
        // A measured empty receiver remains a valid one-check opportunity when
        // its previous decode duration has aged out during idle; this does NOT
        // count as headroom for increasing the continuous capture rate.
        let receiver_ready = match report.receiver {
            ReceiverEvidence::Unobserved => true,
            ReceiverEvidence::Unknown => false,
            ReceiverEvidence::Measured(load) => {
                !load.decoding
                    && load.retained_pictures == 0
                    && load.work_us.is_none_or(|n| n <= report.interval_us * 3 / 4)
            }
        };
        if in_flight
            || report.sample.now_us != now
            || report.sample.send != Availability::Ready
            || report.sample.capture_credit != Availability::Ready
            || report
                .sample
                .source_work_us
                .is_none_or(|n| n > report.interval_us * 3 / 4)
            || !receiver_ready
        {
            return Ok(false);
        }
        let Some(last) = last_capture else {
            return Ok(false);
        };
        let floor = last
            .checked_add(controller.policy().minimum_interval_us)
            .ok_or(Error::Clock)?;
        Ok(now >= floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{ids::*, input_sequence::InputOutcome};
    use fr_media::pacing::{Observation, Policy, Sample};
    use fr_wire::{
        input_result::{InputResult, ResultBinding},
        receiver_metrics::Load,
    };

    #[allow(clippy::unnecessary_wraps)]
    fn result(sequence: u64, space: SequenceSpace, submitted: u32) -> Option<InputReply> {
        Some(InputReply::Record(InputResult {
            binding: ResultBinding {
                channel: 10,
                session: RemoteSessionId::from_raw(1),
                lease: InputLeaseId::from_raw(2),
            },
            sequence,
            space,
            stage: if submitted == 0 {
                Stage::Admitted
            } else {
                Stage::SubmittedToOs
            },
            outcome: if submitted == 0 {
                InputOutcome::RejectedBeforeSubmission
            } else {
                InputOutcome::SubmittedToOs
            },
            submitted_operations: submitted,
            unknown_next_operation: false,
            reason: (submitted == 0).then_some(fr_wire::input_result::Reason::Unsupported),
        }))
    }
    fn sample(t: u64) -> Sample {
        Sample {
            now_us: t,
            source_work_us: Some(1_000),
            send: Availability::Ready,
            capture_credit: Availability::Ready,
            observation: Some(Observation {
                at_us: t,
                changed: false,
            }),
        }
    }
    fn idle() -> Controller {
        let mut c = Controller::new(Policy {
            minimum_interval_us: 20_000,
            maximum_interval_us: 200_000,
        })
        .unwrap();
        for i in 0..=20 {
            c.update(sample(i * 50_000)).unwrap();
        }
        assert_eq!(c.report().unwrap().mode, Mode::Idle);
        c
    }
    #[test]
    fn completed_native_positions_wake_once_and_spaces_do_not_alias() {
        let mut s = Submitted::default();
        assert!(!s.take(None));
        assert!(!s.take(Some(InputReply::CancelledBeforeStart)));
        assert!(s.take(result(0, SequenceSpace::Action, 1)));
        assert!(!s.take(result(0, SequenceSpace::Action, 1)));
        assert!(s.take(result(0, SequenceSpace::Pointer, 1)));
        assert!(s.take(result(2, SequenceSpace::Action, 1)));
        assert!(!s.take(result(1, SequenceSpace::Action, 1)));
        assert!(!s.take(result(3, SequenceSpace::Action, 0)));
        assert!(!s.take(result(2, SequenceSpace::Action, 1)));
        assert!(s.take(result(4, SequenceSpace::Action, 2)));
        assert!(s.take(result(u64::MAX, SequenceSpace::Pointer, 1)));
        assert!(!s.take(result(0, SequenceSpace::Pointer, 1)));
    }
    #[test]
    fn idle_wake_checks_floor_and_consumes_one_credit_without_changing_source_evidence() {
        let mut c = idle();
        let original = c.report();
        let mut w = Wake::default();
        w.note(1_000_000);
        assert!(!w.due(1_000_000, Some(990_000), false, Some(&c)).unwrap());
        assert_eq!(c.report(), original);
        c.update(sample(1_010_000)).unwrap();
        assert!(w.due(1_010_000, Some(990_000), false, Some(&c)).unwrap());
        assert_eq!(c.interval_us(), 200_000);
        assert_eq!(c.report().unwrap().mode, Mode::Idle);
        w.admitted();
        assert!(!w.due(1_010_000, Some(990_000), false, Some(&c)).unwrap());
    }
    #[test]
    fn a_pending_capture_keeps_only_one_post_input_check_and_never_cancels_work() {
        let mut c = idle();
        let mut w = Wake::default();
        for t in [999_000, 999_100, 1_000_000] {
            w.note(t);
        }
        assert!(!w.due(1_000_000, Some(900_000), true, Some(&c)).unwrap());
        c.update(sample(1_020_000)).unwrap();
        assert!(w.due(1_020_000, Some(900_000), false, Some(&c)).unwrap());
        w.admitted();
        assert!(!w.due(1_020_000, Some(900_000), false, Some(&c)).unwrap());
    }
    #[test]
    fn send_retention_source_and_receiver_pressure_each_prevent_early_capture() {
        for cause in 0..7 {
            let mut c = idle();
            let mut w = Wake::default();
            w.note(1_010_000);
            let mut s = sample(1_010_000);
            let mut r = ReceiverEvidence::Unobserved;
            match cause {
                0 => s.send = Availability::Blocked,
                1 => s.capture_credit = Availability::Blocked,
                2 => s.source_work_us = Some(200_000),
                3 => r = ReceiverEvidence::Unknown,
                4 => {
                    r = ReceiverEvidence::Measured(Load {
                        retained_bytes: 1,
                        retained_pictures: 1,
                        decoding: true,
                        work_us: Some(1),
                    });
                }
                5 => {
                    r = ReceiverEvidence::Measured(Load {
                        retained_bytes: 2,
                        retained_pictures: 2,
                        decoding: false,
                        work_us: Some(1),
                    });
                }
                _ => {
                    r = ReceiverEvidence::Measured(Load {
                        retained_bytes: 0,
                        retained_pictures: 0,
                        decoding: false,
                        work_us: Some(200_000),
                    });
                }
            }
            c.update_with_receiver(s, r).unwrap();
            assert!(
                !w.due(1_010_000, Some(900_000), false, Some(&c)).unwrap(),
                "cause {cause}"
            );
            c.update_with_receiver(sample(1_020_000), ReceiverEvidence::Unobserved)
                .unwrap();
            assert!(w.due(1_020_000, Some(900_000), false, Some(&c)).unwrap());
        }
    }
    #[test]
    fn stale_hint_fixed_pacing_and_active_mode_cannot_create_a_later_burst() {
        let mut c = idle();
        let mut w = Wake::default();
        w.note(750_000);
        assert!(!w.due(1_000_000, Some(900_000), false, Some(&c)).unwrap());
        w.note(1_000_000);
        assert!(!w.due(1_000_000, Some(900_000), false, None).unwrap());
        assert!(!w.due(1_000_000, Some(900_000), false, Some(&c)).unwrap());
        w.note(1_010_000);
        let mut s = sample(1_010_000);
        s.observation.as_mut().unwrap().changed = true;
        c.update(s).unwrap();
        assert!(!w.due(1_010_000, Some(900_000), false, Some(&c)).unwrap());
        for i in 1..=23 {
            c.update(sample(1_010_000 + i * 50_000)).unwrap();
        }
        assert!(!w.due(2_160_000, Some(2_050_000), false, Some(&c)).unwrap());
    }
    #[test]
    fn clock_regression_stale_service_measurement_and_floor_overflow_never_wake() {
        let c = idle();
        let mut w = Wake::default();
        w.note(1_000_000);
        assert_eq!(
            w.due(999_999, Some(900_000), false, Some(&c)),
            Err(Error::Clock)
        );
        assert!(!w.due(1_000_001, Some(900_000), false, Some(&c)).unwrap());
        assert_eq!(
            w.due(1_000_000, Some(u64::MAX), false, Some(&c)),
            Err(Error::Clock)
        );
    }
    #[test]
    fn measured_empty_idle_receiver_can_wake_after_old_decode_timing_ages_out() {
        let mut c = idle();
        let mut w = Wake::default();
        w.note(1_010_000);
        c.update_with_receiver(
            sample(1_010_000),
            ReceiverEvidence::Measured(Load {
                retained_bytes: 0,
                retained_pictures: 0,
                decoding: false,
                work_us: None,
            }),
        )
        .unwrap();
        assert!(w.due(1_010_000, Some(900_000), false, Some(&c)).unwrap());
        assert_eq!(c.interval_us(), 200_000);
        assert_eq!(c.report().unwrap().headroom_us, 0);
    }
}
