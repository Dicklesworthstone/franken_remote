//! Capture and bitrate adaptation share reports, but a missing scheduling turn
//! is neither measured congestion nor positive receiver/source evidence.
use fr_media::{
    bitrate::{self, Controller as Bitrate},
    pacing::{
        Availability, Controller, Mode, Observation, Policy, Reason, ReceiverEvidence, Report,
        Sample,
    },
};
use fr_wire::receiver_metrics::Load;

const MINIMUM_US: u64 = 20_000;
const MAXIMUM_US: u64 = 200_000;
const INITIAL_BITRATE: u32 = 8_000_000;

fn controller() -> Controller {
    Controller::new(Policy {
        minimum_interval_us: MINIMUM_US,
        maximum_interval_us: MAXIMUM_US,
    })
    .unwrap()
}

fn active(now_us: u64) -> Sample {
    Sample {
        now_us,
        source_work_us: Some(5_000),
        send: Availability::Ready,
        capture_credit: Availability::Ready,
        observation: Some(Observation {
            at_us: now_us,
            changed: true,
        }),
    }
}

fn unknown(now_us: u64) -> Sample {
    Sample {
        now_us,
        source_work_us: None,
        send: Availability::Unknown,
        capture_credit: Availability::Unknown,
        observation: None,
    }
}

fn update(
    pacing: &mut Controller,
    bitrate: &mut Bitrate,
    sample: Sample,
    receiver: ReceiverEvidence,
) -> Report {
    let report = pacing.update_with_receiver(sample, receiver).unwrap();
    bitrate.observe(&report).unwrap();
    report
}

fn make_idle(pacing: &mut Controller) {
    for now in (0..=1_000_000).step_by(50_000) {
        let mut sample = active(now);
        sample.observation.as_mut().unwrap().changed = false;
        pacing.update(sample).unwrap();
    }
    assert_eq!(pacing.report().unwrap().mode, Mode::Idle);
    assert_eq!(pacing.interval_us(), MAXIMUM_US);
}

#[test]
fn repeated_unknown_gaps_preserve_a_measured_cadence_without_changing_bitrate() {
    let mut pacing = controller();
    let mut bitrate = Bitrate::new(INITIAL_BITRATE).unwrap();
    for now in (0..=15_000_000).step_by(50_000) {
        update(
            &mut pacing,
            &mut bitrate,
            active(now),
            ReceiverEvidence::Unobserved,
        );
    }
    assert_eq!(pacing.interval_us(), MINIMUM_US);
    let mut now = 15_000_000;
    for gap in [100_001, 250_000, 1_000_000, 10_000_000, 100_001] {
        now += gap;
        let report = update(
            &mut pacing,
            &mut bitrate,
            unknown(now),
            ReceiverEvidence::Unknown,
        );
        assert_eq!(report.reason, Reason::EvidenceGap);
        assert_eq!(report.previous_interval_us, MINIMUM_US);
        assert_eq!(report.interval_us, MINIMUM_US);
        assert_eq!(report.pressure_us, [0; 3]);
        assert_eq!(report.receiver_pressure_us, [0; 2]);
        assert_eq!(report.headroom_us, 0);
        assert_eq!(bitrate.pending(), None);
        assert_eq!(bitrate.applied_bitrate(), INITIAL_BITRATE);
    }
}

#[test]
fn pressure_before_a_gap_does_not_count_but_fresh_pressure_still_reduces() {
    let mut pacing = controller();
    let mut bitrate = Bitrate::new(INITIAL_BITRATE).unwrap();
    let receiver = ReceiverEvidence::Measured(Load {
        retained_bytes: 2_048,
        retained_pictures: 2,
        decoding: true,
        work_us: Some(1_000_000),
    });
    for (now, pressure_us) in [
        (0, 0),
        (50_000, 50_000),
        (150_001, 0),
        (200_001, 50_000),
        (250_001, 100_000),
    ] {
        let mut sample = active(now);
        sample.source_work_us = Some(1_000_000);
        sample.send = Availability::Blocked;
        sample.capture_credit = Availability::Blocked;
        let report = update(&mut pacing, &mut bitrate, sample, receiver);
        assert_eq!(report.pressure_us, [pressure_us; 3]);
        assert_eq!(report.receiver_pressure_us, [pressure_us; 2]);
        if now < 250_001 {
            assert_eq!(report.interval_us, 40_000);
            assert_eq!(bitrate.pending(), None);
        } else {
            assert_eq!(report.reason, Reason::SourceWork);
            assert_eq!(report.interval_us, 80_000);
            let pending = bitrate.pending().unwrap();
            assert_eq!(pending.reason(), bitrate::Reason::SendPressure);
            assert_eq!(pending.bitrate(), INITIAL_BITRATE / 2);
            assert_eq!(bitrate.applied_bitrate(), INITIAL_BITRATE);
        }
    }
}

#[test]
fn an_evidence_only_gap_does_not_restart_the_last_real_adjustment_cooldown() {
    let mut pacing = controller();
    let mut bitrate = Bitrate::new(INITIAL_BITRATE).unwrap();
    for now in [0, 50_000, 100_000, 150_000, 250_001, 300_001, 350_001] {
        let mut sample = active(now);
        sample.send = Availability::Blocked;
        let report = update(
            &mut pacing,
            &mut bitrate,
            sample,
            ReceiverEvidence::Unobserved,
        );
        match now {
            100_000 => {
                assert_eq!(report.reason, Reason::SendAdmission);
                assert_eq!(report.interval_us, 80_000);
                let capture = bitrate.begin_capture().unwrap();
                bitrate
                    .complete_capture(capture, INITIAL_BITRATE / 2, true, now)
                    .unwrap();
            }
            250_001 => {
                assert_eq!(report.reason, Reason::EvidenceGap);
                assert_eq!(report.interval_us, 80_000);
                assert_eq!(report.pressure_us[1], 0);
            }
            300_001 => assert_eq!(report.interval_us, 80_000),
            350_001 => {
                // Exactly 100 ms of newly measured pressure, and more than
                // 250 ms since the actual reduction, independently of the gap.
                assert_eq!(report.reason, Reason::SendAdmission);
                assert_eq!(report.interval_us, 160_000);
            }
            _ => {}
        }
        if now >= 100_000 {
            // Bitrate's separate two-second actual-change dwell survives too.
            assert_eq!(bitrate.applied_bitrate(), INITIAL_BITRATE / 2);
            assert_eq!(bitrate.pending(), None);
        }
    }
}

#[test]
fn neither_cadence_nor_bitrate_probes_can_spend_headroom_from_before_a_gap() {
    let mut pacing = controller();
    let mut bitrate = Bitrate::new(INITIAL_BITRATE).unwrap();
    for now in [0, 50_000, 100_000] {
        let mut sample = active(now);
        sample.send = Availability::Blocked;
        update(
            &mut pacing,
            &mut bitrate,
            sample,
            ReceiverEvidence::Unobserved,
        );
    }
    let capture = bitrate.begin_capture().unwrap();
    bitrate
        .complete_capture(capture, INITIAL_BITRATE / 2, true, 100_000)
        .unwrap();
    for now in (150_000..=4_100_000).step_by(50_000) {
        update(
            &mut pacing,
            &mut bitrate,
            active(now),
            ReceiverEvidence::Unobserved,
        );
        assert_eq!(bitrate.pending(), None);
    }
    let established = pacing.interval_us();
    let gap = update(
        &mut pacing,
        &mut bitrate,
        unknown(4_200_001),
        ReceiverEvidence::Unknown,
    );
    assert_eq!(gap.reason, Reason::EvidenceGap);
    assert_eq!(gap.interval_us, established);
    assert_eq!(gap.headroom_us, 0);
    assert_eq!(bitrate.pending(), None);

    for now in (4_250_001..=8_250_001).step_by(50_000) {
        let report = update(
            &mut pacing,
            &mut bitrate,
            active(now),
            ReceiverEvidence::Unobserved,
        );
        if now < 6_250_001 {
            assert_eq!(report.interval_us, established);
            assert_ne!(report.reason, Reason::HeadroomProbe);
        }
        assert_eq!(bitrate.pending(), None);
    }
    update(
        &mut pacing,
        &mut bitrate,
        active(8_300_001),
        ReceiverEvidence::Unobserved,
    );
    let probe = bitrate.pending().unwrap();
    assert_eq!(probe.reason(), bitrate::Reason::Headroom);
    assert_eq!(probe.bitrate(), 4_250_000);
    assert_eq!(bitrate.applied_bitrate(), INITIAL_BITRATE / 2);
}

#[test]
fn an_inflight_upward_target_after_a_gap_retains_its_actual_effect_and_restoration() {
    let mut pacing = Controller::new(Policy {
        minimum_interval_us: 40_000,
        maximum_interval_us: 40_000,
    })
    .unwrap();
    let mut bitrate = Bitrate::new(INITIAL_BITRATE).unwrap();
    for now in [0, 50_000, 100_000] {
        let mut sample = active(now);
        sample.send = Availability::Blocked;
        update(
            &mut pacing,
            &mut bitrate,
            sample,
            ReceiverEvidence::Unobserved,
        );
    }
    let reduction = bitrate.begin_capture().unwrap();
    bitrate
        .complete_capture(reduction, INITIAL_BITRATE / 2, true, 100_000)
        .unwrap();
    for now in (150_000..=4_200_000).step_by(50_000) {
        update(
            &mut pacing,
            &mut bitrate,
            active(now),
            ReceiverEvidence::Unobserved,
        );
    }
    let original = bitrate.begin_capture().unwrap();
    assert_eq!(original.bitrate(), 4_250_000);
    let gap = update(
        &mut pacing,
        &mut bitrate,
        unknown(4_300_001),
        ReceiverEvidence::Unknown,
    );
    assert_eq!(gap.interval_us, 40_000);
    assert_eq!(gap.reason, Reason::EvidenceGap);
    let restoration = bitrate.pending().unwrap();
    assert_eq!(restoration.reason(), bitrate::Reason::EvidenceReset);
    assert_eq!(restoration.bitrate(), INITIAL_BITRATE / 2);
    bitrate
        .complete_capture(original, 4_250_000, true, 4_300_001)
        .unwrap();
    assert_eq!(bitrate.applied_bitrate(), 4_250_000);
    assert_eq!(bitrate.pending(), Some(restoration));
    let restore = bitrate.begin_capture().unwrap();
    assert_eq!(restore.bitrate(), INITIAL_BITRATE / 2);
    bitrate
        .complete_capture(restore, INITIAL_BITRATE / 2, true, 6_300_001)
        .unwrap();
    assert_eq!(bitrate.applied_bitrate(), INITIAL_BITRATE / 2);
    assert_eq!(bitrate.pending(), None);
    assert_eq!(bitrate.ceiling_bitrate(), INITIAL_BITRATE);
}

#[test]
fn genuine_motion_after_idle_wakes_conservatively_even_when_sampling_was_delayed() {
    for gap in [50_000, 100_001, 250_001, 2_000_000] {
        let mut pacing = controller();
        assert_eq!(pacing.interval_us(), 40_000, "startup remains conservative");
        make_idle(&mut pacing);
        let report = pacing.update(active(1_000_000 + gap)).unwrap();
        assert_eq!(report.reason, Reason::ChangedAfterIdle, "gap {gap}");
        assert_eq!(report.mode, Mode::Active);
        assert_eq!(report.interval_us, 40_000);
        assert_eq!(report.headroom_us, 0);
    }
}

#[test]
fn delayed_idle_wake_never_overrides_current_source_sender_or_receiver_pressure() {
    for cause in 0..5 {
        let mut pacing = controller();
        make_idle(&mut pacing);
        let mut sample = active(2_000_000);
        let mut receiver = ReceiverEvidence::Unobserved;
        match cause {
            0 => sample.source_work_us = Some(1_000_000),
            1 => sample.send = Availability::Blocked,
            2 => sample.capture_credit = Availability::Blocked,
            3 | 4 => {
                receiver = ReceiverEvidence::Measured(Load {
                    retained_bytes: 2_048,
                    retained_pictures: if cause == 3 { 2 } else { 1 },
                    decoding: cause == 4,
                    work_us: Some(if cause == 4 { 1_000_000 } else { 5_000 }),
                });
            }
            _ => unreachable!(),
        }
        let report = pacing.update_with_receiver(sample, receiver).unwrap();
        assert_eq!(report.reason, Reason::ChangedAfterIdle, "pressure {cause}");
        assert_eq!(report.mode, Mode::Active);
        assert_eq!(report.interval_us, MAXIMUM_US);
        assert_eq!(report.headroom_us, 0);
    }
}

#[test]
fn missing_duplicate_stale_or_unchanged_pixels_cannot_accelerate_after_an_idle_gap() {
    for observation in [
        None,
        Some(Observation {
            at_us: 1_000_000,
            changed: false,
        }),
        Some(Observation {
            at_us: 1_000_001,
            changed: true,
        }),
        Some(Observation {
            at_us: 2_000_000,
            changed: false,
        }),
    ] {
        let mut pacing = controller();
        make_idle(&mut pacing);
        let mut sample = active(2_000_000);
        sample.observation = observation;
        let report = pacing.update(sample).unwrap();
        assert_eq!(report.reason, Reason::EvidenceGap, "{observation:?}");
        assert_eq!(report.mode, Mode::Active);
        assert_eq!(report.interval_us, MAXIMUM_US);
        assert_eq!(report.headroom_us, 0);
    }
}

#[test]
fn a_later_fresh_change_can_wake_after_unknown_stale_and_duplicate_idle_evidence() {
    let mut pacing = controller();
    make_idle(&mut pacing);
    let gap = pacing.update(unknown(2_000_000)).unwrap();
    assert_eq!(gap.reason, Reason::EvidenceGap);
    assert_eq!(gap.mode, Mode::Active);
    assert_eq!(gap.interval_us, MAXIMUM_US);

    // The delayed capture is already stale, and replaying its exact metadata
    // does not make it fresh. Neither consumes the original idle wake intent.
    for now in [2_050_000, 2_100_000] {
        let mut sample = active(now);
        sample.observation = Some(Observation {
            at_us: 1_000_001,
            changed: true,
        });
        let report = pacing.update(sample).unwrap();
        assert_eq!(report.interval_us, MAXIMUM_US);
        assert_ne!(report.reason, Reason::ChangedAfterIdle);
        assert_eq!(report.headroom_us, 0);
    }

    let mut unchanged = active(2_150_000);
    unchanged.observation.as_mut().unwrap().changed = false;
    let report = pacing.update(unchanged).unwrap();
    assert_eq!(report.interval_us, MAXIMUM_US);
    assert_ne!(report.reason, Reason::ChangedAfterIdle);

    // This independently observed change arrived on a later service turn than
    // the scheduling gap. The original capture cadence may now wake once.
    let wake = pacing.update(active(2_200_000)).unwrap();
    assert_eq!(wake.reason, Reason::ChangedAfterIdle);
    assert_eq!(wake.mode, Mode::Active);
    assert_eq!(wake.interval_us, 40_000);
    assert_eq!(wake.headroom_us, 0);
    assert_eq!(
        pacing.update(active(2_250_000)).unwrap().reason,
        Reason::Hold
    );
}

#[test]
fn a_pressure_veto_consumes_the_idle_wake_instead_of_retrying_on_a_later_change() {
    let mut pacing = controller();
    make_idle(&mut pacing);
    pacing.update(unknown(2_000_000)).unwrap();
    let mut wake = active(2_050_000);
    wake.send = Availability::Blocked;
    let refused_acceleration = pacing.update(wake).unwrap();
    assert_eq!(refused_acceleration.reason, Reason::ChangedAfterIdle);
    assert_eq!(refused_acceleration.interval_us, MAXIMUM_US);

    // Every later sample is genuine new motion with known headroom. It must
    // earn the ordinary probe dwell, never retry the already-consumed wake.
    for now in (2_100_000..4_100_000).step_by(50_000) {
        let report = pacing.update(active(now)).unwrap();
        assert_eq!(report.reason, Reason::Hold);
        assert_eq!(report.interval_us, MAXIMUM_US);
    }
    let probe = pacing.update(active(4_100_000)).unwrap();
    assert_eq!(probe.reason, Reason::HeadroomProbe);
    assert_eq!(probe.interval_us, 175_000);
}
