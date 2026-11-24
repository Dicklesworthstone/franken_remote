//! The default native cadence changes only after measured receiver overload.
use fr_media::pacing::{
    Availability, Controller, Error, Observation, Policy, Reason, ReceiverEvidence, Sample,
};
use fr_wire::receiver_metrics::Load;

const MINIMUM: u64 = 50_000;
const MAXIMUM: u64 = 200_000;

fn controller() -> Controller {
    Controller::for_receiver_overload(Policy {
        minimum_interval_us: MINIMUM,
        maximum_interval_us: MAXIMUM,
    })
    .unwrap()
}

fn sample(now_us: u64) -> Sample {
    Sample {
        now_us,
        source_work_us: Some(1_000),
        send: Availability::Ready,
        capture_credit: Availability::Ready,
        observation: Some(Observation {
            at_us: now_us,
            changed: true,
        }),
    }
}

fn healthy() -> Load {
    Load {
        retained_bytes: 1024,
        retained_pictures: 1,
        decoding: false,
        work_us: Some(1_000),
    }
}

fn backlog() -> ReceiverEvidence {
    ReceiverEvidence::Measured(Load {
        retained_pictures: 2,
        retained_bytes: 2048,
        ..healthy()
    })
}

#[test]
fn dormant_rescue_preserves_baseline_for_healthy_unknown_and_unnegotiated_receivers() {
    for evidence in [
        ReceiverEvidence::Measured(healthy()),
        ReceiverEvidence::Unknown,
        ReceiverEvidence::Unobserved,
    ] {
        let mut controller = controller();
        assert!(controller.is_receiver_rescue());
        assert_eq!(controller.interval_us(), MINIMUM);
        for now in (0..=5_000_000).step_by(20_000) {
            let mut current = sample(now);
            // These retain their independent existing admission/backpressure
            // gates, but cannot activate a receiver-only rescue policy.
            current.source_work_us = Some(500_000);
            current.capture_credit = Availability::Blocked;
            current.send = Availability::Blocked;
            current.observation.as_mut().unwrap().changed = false;
            let report = controller.update_with_receiver(current, evidence).unwrap();
            assert_eq!(report.interval_us, MINIMUM);
            assert_eq!(report.reason, Reason::Hold);
        }
        assert_eq!(controller.decisions().count(), 0);
    }
}

#[test]
fn each_receiver_pressure_kind_requires_one_second_of_measured_evidence() {
    for (load, reason) in [
        (backlog(), Reason::ReceiverBacklog),
        (
            ReceiverEvidence::Measured(Load {
                decoding: true,
                work_us: Some(250_000),
                ..healthy()
            }),
            Reason::DecoderWork,
        ),
    ] {
        let mut controller = controller();
        for now in (0..1_000_000).step_by(20_000) {
            let report = controller.update_with_receiver(sample(now), load).unwrap();
            assert_eq!(report.interval_us, MINIMUM);
            assert_eq!(report.reason, Reason::Hold);
        }
        let first = controller
            .update_with_receiver(sample(1_000_000), load)
            .unwrap();
        assert_eq!(first.reason, reason);
        assert_eq!(first.interval_us, 2 * MINIMUM);
        for now in (1_020_000..1_250_000).step_by(20_000) {
            assert_eq!(
                controller
                    .update_with_receiver(sample(now), load)
                    .unwrap()
                    .interval_us,
                2 * MINIMUM
            );
        }
        let next = controller
            .update_with_receiver(sample(1_250_000), load)
            .unwrap();
        assert_eq!(next.reason, reason);
        assert_eq!(next.interval_us, MAXIMUM);
    }
}

#[test]
fn brief_unknown_intervals_pause_without_counting_either_unknown_edge() {
    let mut controller = controller();
    let mut measured_us = 0;
    let mut previous_busy = false;
    let mut engaged = false;
    for now in (0..=3_000_000).step_by(20_000) {
        // Each 100 ms window contains two measured 20 ms intervals, followed
        // by an unknown gap shorter than the original 100 ms grace.
        let busy = now % 100_000 <= 40_000;
        if busy && previous_busy {
            measured_us += 20_000;
        }
        let evidence = if busy {
            backlog()
        } else {
            ReceiverEvidence::Unknown
        };
        let report = controller
            .update_with_receiver(sample(now), evidence)
            .unwrap();
        if !engaged {
            if measured_us < 1_000_000 || !busy {
                assert_eq!(report.interval_us, MINIMUM);
            } else {
                assert_eq!(report.reason, Reason::ReceiverBacklog);
                assert_eq!(report.receiver_pressure_us[0], 1_000_000);
                assert_eq!(report.interval_us, 2 * MINIMUM);
                engaged = true;
            }
        }
        previous_busy = busy;
    }
    assert!(
        engaged,
        "bounded gaps may pause evidence, not erase every live window"
    );
}

#[test]
fn counterevidence_long_unknown_gaps_and_new_receiver_scope_restart_engagement() {
    for reset in 0..4 {
        let mut controller = controller();
        for now in (0..=900_000).step_by(20_000) {
            controller
                .update_with_receiver(sample(now), backlog())
                .unwrap();
        }
        let resumed = match reset {
            0 => {
                controller
                    .update_with_receiver(sample(920_000), ReceiverEvidence::Measured(healthy()))
                    .unwrap();
                940_000
            }
            1 => {
                controller
                    .update_with_receiver(sample(920_000), ReceiverEvidence::Unobserved)
                    .unwrap();
                940_000
            }
            2 => {
                for now in (920_000..=1_020_000).step_by(20_000) {
                    controller
                        .update_with_receiver(sample(now), ReceiverEvidence::Unknown)
                        .unwrap();
                }
                1_040_000
            }
            _ => {
                controller.reset_receiver_evidence();
                920_000
            }
        };
        for now in (resumed..resumed + 1_000_000).step_by(20_000) {
            assert_eq!(
                controller
                    .update_with_receiver(sample(now), backlog())
                    .unwrap()
                    .interval_us,
                MINIMUM
            );
        }
        assert_eq!(
            controller
                .update_with_receiver(sample(resumed + 1_000_000), backlog())
                .unwrap()
                .interval_us,
            2 * MINIMUM
        );
    }
}

#[test]
fn recovery_requires_measured_headroom_and_disengages_only_at_the_original_baseline() {
    let mut controller = controller();
    for now in (0..=1_500_000).step_by(20_000) {
        controller
            .update_with_receiver(sample(now), backlog())
            .unwrap();
    }
    assert_eq!(controller.interval_us(), MAXIMUM);
    for now in (1_520_000..=7_500_000).step_by(20_000) {
        let evidence = if now < 4_500_000 {
            ReceiverEvidence::Unknown
        } else {
            ReceiverEvidence::Unobserved
        };
        assert_eq!(
            controller
                .update_with_receiver(sample(now), evidence)
                .unwrap()
                .interval_us,
            MAXIMUM,
            "missing or unnegotiated feedback cannot prove the decoder recovered"
        );
    }
    let mut reached_baseline = None;
    let mut previous_probe = None;
    for now in (7_520_000..=45_000_000).step_by(20_000) {
        let report = controller
            .update_with_receiver(sample(now), ReceiverEvidence::Measured(healthy()))
            .unwrap();
        if report.reason == Reason::HeadroomProbe {
            assert!(report.headroom_us >= 2_000_000);
            assert_eq!(
                report.interval_us,
                (report.previous_interval_us - report.previous_interval_us.div_ceil(8))
                    .max(MINIMUM)
            );
            if let Some(previous) = previous_probe {
                assert!(now - previous >= 2_000_000);
            }
            previous_probe = Some(now);
        }
        if report.interval_us == MINIMUM {
            reached_baseline = Some(now);
            break;
        }
    }
    let reached = reached_baseline.expect("measured recovery returns to configured cadence");
    // A later short overload has to earn the full engagement dwell again.
    for now in (reached + 20_000..reached + 1_020_000).step_by(20_000) {
        assert_eq!(
            controller
                .update_with_receiver(sample(now), backlog())
                .unwrap()
                .interval_us,
            MINIMUM
        );
    }
    assert_eq!(
        controller
            .update_with_receiver(sample(reached + 1_020_000), backlog())
            .unwrap()
            .interval_us,
        2 * MINIMUM
    );
}

#[test]
fn explicit_adaptive_behavior_and_terminal_clock_errors_are_preserved() {
    let policy = controller().policy();
    let mut adaptive = Controller::new(policy).unwrap();
    assert!(!adaptive.is_receiver_rescue());
    assert_eq!(adaptive.interval_us(), 2 * MINIMUM);
    for now in (0..100_000).step_by(20_000) {
        adaptive
            .update_with_receiver(sample(now), backlog())
            .unwrap();
    }
    assert_eq!(
        adaptive
            .update_with_receiver(sample(100_000), backlog())
            .unwrap()
            .interval_us,
        MAXIMUM
    );
    let mut rescue = controller();
    rescue
        .update_with_receiver(sample(50_000), backlog())
        .unwrap();
    assert_eq!(
        rescue.update_with_receiver(sample(49_999), backlog()),
        Err(Error::ClockRegression)
    );
    assert_eq!(rescue.update(sample(50_001)), Err(Error::Closed));
}

#[test]
fn an_old_idle_wake_cannot_reengage_rescue_after_measured_recovery() {
    let mut controller = controller();
    for now in (0..=1_500_000).step_by(20_000) {
        controller
            .update_with_receiver(sample(now), backlog())
            .unwrap();
    }
    for now in (1_520_000..=2_600_000).step_by(20_000) {
        let mut current = sample(now);
        current.observation.as_mut().unwrap().changed = false;
        controller
            .update_with_receiver(current, ReceiverEvidence::Measured(healthy()))
            .unwrap();
    }
    assert_eq!(
        controller.report().unwrap().mode,
        fr_media::pacing::Mode::Idle
    );
    // A real service gap leaves the old wake opportunity pending. Intermittent
    // unchanged observations then supply headroom without a changed-image wake.
    let start = 3_000_000;
    let mut recovered = None;
    for now in (start..=40_000_000).step_by(20_000) {
        let mut current = sample(now);
        current.observation = (now - start)
            .is_multiple_of(300_000)
            .then_some(Observation {
                at_us: now,
                changed: false,
            });
        controller
            .update_with_receiver(current, ReceiverEvidence::Measured(healthy()))
            .unwrap();
        if controller.interval_us() == MINIMUM {
            recovered = Some(now);
            break;
        }
    }
    let recovered = recovered.expect("fresh bounded headroom returns to baseline");
    let changed = controller
        .update_with_receiver(
            sample(recovered + 20_000),
            ReceiverEvidence::Measured(healthy()),
        )
        .unwrap();
    assert_eq!(changed.reason, Reason::Hold);
    assert_eq!(changed.interval_us, MINIMUM);
}
