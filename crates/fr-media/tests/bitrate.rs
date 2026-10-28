use fr_media::{
    bitrate::{Controller, Error, Reason},
    pacing::{
        Availability as A, Controller as Pacer, Mode, Observation, Policy, ReceiverEvidence,
        Report, Sample,
    },
};
use fr_wire::receiver_metrics::Load;

fn pacer() -> Pacer {
    Pacer::new(Policy {
        minimum_interval_us: 40_000,
        maximum_interval_us: 40_000,
    })
    .unwrap()
}

fn sample(now_us: u64, send: A) -> Sample {
    Sample {
        now_us,
        source_work_us: Some(5_000),
        send,
        capture_credit: A::Ready,
        observation: Some(Observation {
            at_us: now_us,
            changed: true,
        }),
    }
}

fn observe(c: &mut Controller, p: &mut Pacer, now: u64, send: A) -> Report {
    let report = p.update(sample(now, send)).unwrap();
    c.observe(&report).unwrap();
    report
}

fn proposed_cut(initial: u32) -> (Controller, Pacer) {
    let mut c = Controller::new(initial).unwrap();
    let mut p = pacer();
    for now in [0, 50_000, 100_000] {
        observe(&mut c, &mut p, now, A::Blocked);
    }
    (c, p)
}

fn reduced() -> (Controller, Pacer) {
    let (mut c, p) = proposed_cut(8_000_000);
    let capture = c.begin_capture().unwrap();
    assert_eq!(capture.bitrate(), 4_000_000);
    c.complete_capture(capture, 4_000_000, true, 100_000)
        .unwrap();
    (c, p)
}

#[test]
fn sustained_sender_pressure_reduces_bytes_even_at_maximum_capture_interval() {
    let mut p = Pacer::new(Policy {
        minimum_interval_us: 20_000,
        maximum_interval_us: 200_000,
    })
    .unwrap();
    let mut c = Controller::new(8_000_000).unwrap();
    for now in [0, 50_000] {
        observe(&mut c, &mut p, now, A::Blocked);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 100_000, A::Blocked);
    let change = c.pending().unwrap();
    assert_eq!(change.reason(), Reason::SendPressure);
    assert_eq!(change.bitrate(), 4_000_000);
    assert_eq!(c.applied_bitrate(), 8_000_000);
    for now in (150_000..=1_000_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Blocked);
        let capture = c.begin_capture().unwrap();
        // An unchanged screen or native dwell may defer the same target.
        c.complete_capture(capture, 8_000_000, false, now).unwrap();
        assert_eq!(c.pending(), Some(change));
        assert_eq!(c.applied_bitrate(), 8_000_000);
    }
    assert_eq!(p.interval_us(), 200_000);
    let capture = c.begin_capture().unwrap();
    c.complete_capture(capture, 4_000_000, true, 1_000_000)
        .unwrap();
    for now in (1_050_000..3_000_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Blocked);
        assert_eq!(c.pending(), None, "dwell starts at actual native output");
    }
    let report = observe(&mut c, &mut p, 3_000_000, A::Blocked);
    assert_eq!(report.reason, fr_media::pacing::Reason::Hold);
    assert_eq!(report.interval_us, 200_000);
    let next = c.pending().unwrap();
    assert_ne!(next.id(), change.id());
    assert_eq!(next.bitrate(), 2_000_000);
}

#[test]
fn local_work_credit_and_receiver_pressure_alone_never_cut_bitrate() {
    for cause in 0..5 {
        let mut c = Controller::new(8_000_000).unwrap();
        let mut p = pacer();
        for now in (0..=10_000_000).step_by(50_000) {
            let mut s = sample(now, A::Ready);
            let mut receiver = ReceiverEvidence::Unobserved;
            match cause {
                0 => s.source_work_us = Some(500_000),
                1 => s.capture_credit = A::Blocked,
                2 => {
                    receiver = ReceiverEvidence::Measured(Load {
                        retained_bytes: 2_048,
                        retained_pictures: 2,
                        decoding: false,
                        work_us: Some(5_000),
                    });
                }
                3 => {
                    receiver = ReceiverEvidence::Measured(Load {
                        retained_bytes: 1_024,
                        retained_pictures: 1,
                        decoding: true,
                        work_us: Some(500_000),
                    });
                }
                _ => s.send = A::Unknown,
            }
            c.observe(&p.update_with_receiver(s, receiver).unwrap())
                .unwrap();
            assert_eq!(c.pending(), None, "non-send pressure {cause}");
        }
        assert_eq!(c.applied_bitrate(), 8_000_000);
    }
}

#[test]
fn verified_idle_does_not_build_sender_pressure_or_claim_headroom() {
    let mut p = pacer();
    let mut c = Controller::new(8_000_000).unwrap();
    for now in (0..=1_000_000).step_by(50_000) {
        let mut s = sample(now, A::Ready);
        s.observation.as_mut().unwrap().changed = false;
        p.update(s).unwrap();
    }
    for now in (1_050_000..=6_000_000).step_by(50_000) {
        let mut s = sample(now, A::Blocked);
        s.observation.as_mut().unwrap().changed = false;
        let report = p.update(s).unwrap();
        assert_eq!(report.mode, Mode::Idle);
        c.observe(&report).unwrap();
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn unknown_sender_intervals_and_duplicate_times_cannot_manufacture_pressure() {
    let mut c = Controller::new(8_000_000).unwrap();
    let mut p = pacer();
    for i in 0..400 {
        let send = if i % 4 < 3 { A::Blocked } else { A::Unknown };
        let report = observe(&mut c, &mut p, i * 25_000, send);
        for _ in 0..4 {
            c.observe(&report).unwrap();
        }
        assert_eq!(c.pending(), None);
    }
    let mut c = Controller::new(8_000_000).unwrap();
    let mut p = pacer();
    for _ in 0..100 {
        observe(&mut c, &mut p, 0, A::Blocked);
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn positive_sender_evidence_has_an_exact_bounded_gap() {
    for (gap, proposed) in [(100_000, true), (100_001, false)] {
        let mut c = Controller::new(8_000_000).unwrap();
        let mut p = pacer();
        observe(&mut c, &mut p, 0, A::Blocked);
        observe(&mut c, &mut p, gap, A::Blocked);
        assert_eq!(c.pending().is_some(), proposed);
    }
}

#[test]
fn intervening_native_completions_do_not_fill_missing_send_observations() {
    let mut c = Controller::new(8_000_000).unwrap();
    let mut p = pacer();
    observe(&mut c, &mut p, 0, A::Blocked);
    for now in [50_000, 100_000, 150_000] {
        let capture = c.begin_capture().unwrap();
        c.complete_capture(capture, 8_000_000, false, now).unwrap();
    }
    // Use a separately valid report stream to prove the bitrate owner enforces
    // its own observation gap, not merely a pacing Reason::EvidenceGap label.
    for now in [50_000, 100_000, 150_000] {
        p.update(sample(now, A::Blocked)).unwrap();
    }
    let report = p.update(sample(200_000, A::Blocked)).unwrap();
    assert_ne!(report.reason, fr_media::pacing::Reason::EvidenceGap);
    c.observe(&report).unwrap();
    assert_eq!(c.pending(), None);
    observe(&mut c, &mut p, 250_000, A::Blocked);
    assert_eq!(c.pending(), None);
    observe(&mut c, &mut p, 300_000, A::Blocked);
    assert_eq!(c.pending().unwrap().bitrate(), 4_000_000);
}

#[test]
fn bitrate_floor_and_original_ceiling_survive_repeated_congestion() {
    for initial in [10_000, 99_999, 100_000, 100_001, 8_000_000, 200_000_000] {
        let mut c = Controller::new(initial).unwrap();
        let mut p = pacer();
        let floor = (initial / 8).max(100_000).min(initial);
        let mut last_change = None;
        assert_eq!(c.ceiling_bitrate(), initial);
        assert_eq!(c.minimum_bitrate(), floor);
        for now in (0..=8_000_000).step_by(50_000) {
            observe(&mut c, &mut p, now, A::Blocked);
            if let Some(change) = c.pending() {
                assert_eq!(change.reason(), Reason::SendPressure);
                assert_eq!(change.bitrate(), (c.applied_bitrate() / 2).max(floor));
                assert!(last_change.is_none_or(|last| now - last >= 2_000_000));
                let capture = c.begin_capture().unwrap();
                c.complete_capture(capture, change.bitrate(), true, now)
                    .unwrap();
                last_change = Some(now);
            }
            assert!((floor..=initial).contains(&c.applied_bitrate()));
        }
        assert_eq!(c.applied_bitrate(), floor);
    }
    for invalid in [0, 9_999, 200_000_001, u32::MAX] {
        assert_eq!(Controller::new(invalid).unwrap_err(), Error::InvalidBitrate);
    }
}

#[test]
fn headroom_probes_are_slow_small_and_capped_by_original_configuration() {
    let (mut c, mut p) = reduced();
    let mut previous_change = 100_000;
    let mut probes = 0;
    for now in (150_000..=60_000_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
        if let Some(change) = c.pending() {
            probes += 1;
            assert_eq!(change.reason(), Reason::Headroom);
            assert!(now - previous_change >= 4_000_000);
            assert_eq!(
                change.bitrate(),
                (c.applied_bitrate() + c.applied_bitrate().div_ceil(16)).min(8_000_000)
            );
            let capture = c.begin_capture().unwrap();
            c.complete_capture(capture, change.bitrate(), true, now)
                .unwrap();
            previous_change = now;
        }
        assert!(c.applied_bitrate() <= 8_000_000);
    }
    assert!(probes > 1);
    assert_eq!(c.applied_bitrate(), 8_000_000);
    assert_eq!(c.pending(), None);
}

#[test]
fn first_upward_probe_needs_four_seconds_of_actual_positive_edges() {
    let (mut c, mut p) = reduced();
    for now in (150_000..4_200_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 4_200_000, A::Ready);
    assert_eq!(c.pending().unwrap().bitrate(), 4_250_000);
    assert_eq!(c.applied_bitrate(), 4_000_000);
}

#[test]
fn bounded_unknown_headroom_pauses_without_counting_either_adjacent_edge() {
    let (mut c, mut p) = reduced();
    for now in (150_000..=2_200_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
    }
    observe(&mut c, &mut p, 2_250_000, A::Unknown);
    for now in (2_300_000..4_300_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 4_300_000, A::Ready);
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
}

#[test]
fn long_unknown_headroom_erases_the_old_probe_evidence() {
    let (mut c, mut p) = reduced();
    for now in (150_000..=2_200_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
    }
    for now in [2_250_000, 2_300_000, 2_350_000] {
        observe(&mut c, &mut p, now, A::Unknown);
    }
    for now in (2_400_000..6_450_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 6_450_000, A::Ready);
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
}

#[test]
fn missing_remote_or_local_measurements_cannot_restore_quality() {
    for missing in 0..5 {
        let (mut c, mut p) = reduced();
        for now in (150_000..=10_000_000).step_by(50_000) {
            let mut s = sample(now, A::Ready);
            let mut receiver = ReceiverEvidence::Unobserved;
            match missing {
                0 => s.source_work_us = None,
                1 => s.send = A::Unknown,
                2 => s.capture_credit = A::Unknown,
                3 => receiver = ReceiverEvidence::Unknown,
                _ => {
                    if now % 100_000 == 0 {
                        s.send = A::Unknown;
                    }
                }
            }
            let report = p.update_with_receiver(s, receiver).unwrap();
            c.observe(&report).unwrap();
            assert_eq!(c.pending(), None);
            assert_eq!(c.applied_bitrate(), 4_000_000);
        }
    }
}

#[test]
fn evidence_reset_cancels_unsent_changes_and_requires_fresh_pressure() {
    let mut c = Controller::new(8_000_000).unwrap();
    let mut p = pacer();
    observe(&mut c, &mut p, 0, A::Blocked);
    observe(&mut c, &mut p, 50_000, A::Blocked);
    c.reset_evidence().unwrap();
    for now in [100_000, 150_000] {
        observe(&mut c, &mut p, now, A::Blocked);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 200_000, A::Blocked);
    assert!(c.pending().is_some());
    c.reset_evidence().unwrap();
    assert_eq!(c.pending(), None);
    let capture = c.begin_capture().unwrap();
    assert_eq!(capture.bitrate(), 8_000_000);
    c.complete_capture(capture, 8_000_000, false, 200_000)
        .unwrap();
    assert_eq!(c.applied_bitrate(), 8_000_000);
}

#[test]
fn evidence_reset_does_not_transfer_headroom_between_membership_scopes() {
    let (mut c, mut p) = reduced();
    for now in (150_000..=3_200_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
    }
    c.reset_evidence().unwrap();
    for now in (3_250_000..7_250_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Ready);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 7_250_000, A::Ready);
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
}

#[test]
fn scope_reset_supersedes_a_retained_native_target_with_explicit_current_rate() {
    let (mut c, _) = proposed_cut(8_000_000);
    let target = c.pending().unwrap();
    let capture = c.begin_capture().unwrap();
    c.complete_capture(capture, 8_000_000, false, 150_000)
        .unwrap();
    assert_eq!(c.pending(), Some(target));
    c.reset_evidence().unwrap();
    let cancel = c.pending().unwrap();
    assert_ne!(cancel.id(), target.id());
    assert_eq!(cancel.reason(), Reason::EvidenceReset);
    assert_eq!(cancel.bitrate(), 8_000_000);
    c.reset_evidence().unwrap();
    assert_eq!(c.pending(), Some(cancel));
    let capture = c.begin_capture().unwrap();
    assert_eq!(capture.bitrate(), 8_000_000);
    c.complete_capture(capture, 8_000_000, false, 200_000)
        .unwrap();
    assert_eq!(c.pending(), None);
    assert_eq!(c.applied_bitrate(), 8_000_000);
    let next = c.begin_capture().unwrap();
    assert_eq!(next.bitrate(), 8_000_000);
}

#[test]
fn old_deferred_reply_cannot_acknowledge_an_unsubmitted_cancellation() {
    let (mut c, _) = proposed_cut(8_000_000);
    let old = c.begin_capture().unwrap();
    c.reset_evidence().unwrap();
    let cancel = c.pending().unwrap();
    c.complete_capture(old, 8_000_000, false, 150_000).unwrap();
    assert_eq!(c.pending(), Some(cancel));
    let explicit = c.begin_capture().unwrap();
    assert_eq!(explicit.bitrate(), 8_000_000);
    c.complete_capture(explicit, 8_000_000, false, 200_000)
        .unwrap();
    assert_eq!(c.pending(), None);
}

#[test]
fn old_in_flight_application_is_recorded_then_restored_without_false_cancellation() {
    let (mut c, _) = proposed_cut(8_000_000);
    let old = c.begin_capture().unwrap();
    c.reset_evidence().unwrap();
    let restore = c.pending().unwrap();
    c.complete_capture(old, 4_000_000, true, 150_000).unwrap();
    assert_eq!(c.applied_bitrate(), 4_000_000);
    assert_eq!(c.pending(), Some(restore));
    c.reset_evidence().unwrap();
    assert_eq!(c.pending(), Some(restore));
    let restoration = c.begin_capture().unwrap();
    assert_eq!(restoration.bitrate(), 8_000_000);
    // Native dual-clock dwell can defer restoration, so retain its identity.
    c.complete_capture(restoration, 4_000_000, false, 200_000)
        .unwrap();
    assert_eq!(c.pending(), Some(restore));
    let restoration = c.begin_capture().unwrap();
    c.complete_capture(restoration, 8_000_000, true, 2_150_000)
        .unwrap();
    assert_eq!(c.applied_bitrate(), 8_000_000);
    assert_eq!(c.pending(), None);
}

#[test]
fn actual_output_and_single_capture_identity_are_required_for_application() {
    let (mut c, _) = proposed_cut(8_000_000);
    let original = c.begin_capture().unwrap();
    assert_eq!(c.begin_capture(), Err(Error::CaptureInFlight));
    assert_eq!(
        c.complete_capture(original, 6_000_000, true, 150_000),
        Err(Error::UnexpectedBitrate)
    );
    assert_eq!(
        c.complete_capture(original, 4_000_000, false, 150_000),
        Err(Error::MissingIdr)
    );
    assert_eq!(c.applied_bitrate(), 8_000_000);
    c.complete_capture(original, 8_000_000, false, 150_000)
        .unwrap();
    let next = c.begin_capture().unwrap();
    assert_eq!(
        c.complete_capture(original, 4_000_000, true, 200_000),
        Err(Error::UnexpectedCapture)
    );
    assert_eq!(c.applied_bitrate(), 8_000_000);
    c.complete_capture(next, 4_000_000, true, 200_000).unwrap();
    assert_eq!(c.applied_bitrate(), 4_000_000);
    assert_eq!(
        c.complete_capture(next, 4_000_000, true, 250_000),
        Err(Error::UnexpectedCapture)
    );
}

#[test]
fn clock_regression_closes_the_policy_without_claiming_a_codec_change() {
    let (mut c, _) = proposed_cut(8_000_000);
    let capture = c.begin_capture().unwrap();
    assert_eq!(
        c.complete_capture(capture, 4_000_000, true, 99_999),
        Err(Error::ClockRegression)
    );
    assert_eq!(c.applied_bitrate(), 8_000_000);
    assert_eq!(c.pending(), None);
    assert_eq!(c.begin_capture(), Err(Error::Closed));
    assert_eq!(c.reset_evidence(), Err(Error::Closed));
    assert_eq!(
        c.complete_capture(capture, 4_000_000, true, 200_000),
        Err(Error::Closed)
    );
    let mut p = pacer();
    let report = p.update(sample(200_000, A::Blocked)).unwrap();
    assert_eq!(c.observe(&report), Err(Error::Closed));
}

#[test]
fn evidence_reset_preserves_actual_change_dwell_and_configured_cap() {
    let (mut c, mut p) = reduced();
    c.reset_evidence().unwrap();
    assert_eq!(c.ceiling_bitrate(), 8_000_000);
    assert_eq!(c.minimum_bitrate(), 1_000_000);
    assert_eq!(c.applied_bitrate(), 4_000_000);
    for now in (150_000..2_100_000).step_by(50_000) {
        observe(&mut c, &mut p, now, A::Blocked);
        assert_eq!(c.pending(), None);
    }
    observe(&mut c, &mut p, 2_100_000, A::Blocked);
    assert_eq!(c.pending().unwrap().bitrate(), 2_000_000);
}
