//! Deterministic production-controller traces, not native encoder or link measurements.
use fr_media::{
    bitrate::{self, PathEvidence},
    pacing::{self, Availability as A, Mode, Observation, Policy, ReceiverEvidence, Sample},
    quality::WorkstationProfile,
};
use fr_wire::receiver_metrics::Load;

const PROFILES: [WorkstationProfile; 3] = [
    WorkstationProfile::Auto,
    WorkstationProfile::Text,
    WorkstationProfile::Motion,
];
const INITIAL: u32 = 8_000_000;

fn policy() -> Policy {
    Policy {
        minimum_interval_us: 20_000,
        maximum_interval_us: 200_000,
    }
}

fn controllers(profile: WorkstationProfile) -> (pacing::Controller, bitrate::Controller) {
    (
        pacing::Controller::new_with_profile(policy(), profile).unwrap(),
        bitrate::Controller::new_with_profile(INITIAL, profile).unwrap(),
    )
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

fn observe(
    pacing: &mut pacing::Controller,
    rate: &mut bitrate::Controller,
    now_us: u64,
    send: A,
) -> pacing::Report {
    let report = pacing.update(sample(now_us, send)).unwrap();
    rate.observe(&report).unwrap();
    report
}

#[test]
fn text_and_motion_prioritize_different_real_load_controls_with_bounded_fallback() {
    for (profile, cadence_at, bitrate_at) in [
        (WorkstationProfile::Auto, 100_000, 100_000),
        (WorkstationProfile::Text, 100_000, 250_000),
        (WorkstationProfile::Motion, 250_000, 100_000),
    ] {
        let (mut pacing, mut rate) = controllers(profile);
        let mut first_cadence = None;
        let mut first_rate = None;
        for now in (0..=300_000).step_by(50_000) {
            let report = observe(&mut pacing, &mut rate, now, A::Blocked);
            if report.reason == pacing::Reason::SendAdmission {
                first_cadence.get_or_insert(now);
            }
            if let Some(change) = rate.pending() {
                first_rate.get_or_insert(now);
                assert_eq!(change.reason(), bitrate::Reason::SendPressure);
                assert_eq!(change.bitrate(), INITIAL / 2);
            }
            // A proposal is never misreported as an applied codec change.
            assert_eq!(rate.applied_bitrate(), INITIAL);
        }
        assert_eq!(first_cadence, Some(cadence_at), "{profile:?}");
        assert_eq!(first_rate, Some(bitrate_at), "{profile:?}");
        assert_eq!(pacing.interval_us(), 80_000);
        assert_eq!(pacing.profile(), profile);
        assert_eq!(rate.profile(), profile);
    }
}

#[test]
fn successful_preferred_response_preserves_the_other_quality_dimension() {
    for profile in [WorkstationProfile::Text, WorkstationProfile::Motion] {
        let (mut pacing, mut rate) = controllers(profile);
        for now in [0, 50_000, 100_000] {
            observe(&mut pacing, &mut rate, now, A::Blocked);
        }
        if profile == WorkstationProfile::Motion {
            let capture = rate.begin_capture().unwrap();
            assert_eq!(capture.bitrate(), INITIAL / 2);
            rate.complete_capture(capture, INITIAL / 2, true, 100_000)
                .unwrap();
        } else {
            assert_eq!(rate.pending(), None);
        }
        // New measured admission, not elapsed time or an assumed encoder
        // effect, proves that the first response relieved sender pressure.
        for now in (150_000..=1_000_000).step_by(50_000) {
            let report = observe(&mut pacing, &mut rate, now, A::Ready);
            assert_eq!(report.reason, pacing::Reason::Hold);
            assert_eq!(rate.pending(), None);
        }
        let (interval, bitrate) = if profile == WorkstationProfile::Text {
            (80_000, INITIAL)
        } else {
            (40_000, INITIAL / 2)
        };
        assert_eq!(pacing.interval_us(), interval);
        assert_eq!(rate.applied_bitrate(), bitrate);
    }
}

#[test]
fn queued_path_overrides_text_preference_and_keeps_the_measured_delivery_cap() {
    for profile in PROFILES {
        let (mut pacing, mut rate) = controllers(profile);
        for now in (0..=200_000).step_by(50_000) {
            // Text has a genuine local send backlog as well. It must not wait
            // for its 250 ms preference when path evidence demands a cut.
            let send = if profile == WorkstationProfile::Text {
                A::Blocked
            } else {
                A::Ready
            };
            observe(&mut pacing, &mut rate, now, send);
            rate.observe_path_evidence(
                now,
                PathEvidence::Queued {
                    delivered_bits_per_second: 2_000_000,
                },
            )
            .unwrap();
            if now < 200_000 {
                assert_eq!(rate.pending(), None);
            }
        }
        let change = rate.pending().unwrap();
        assert_eq!(change.reason(), bitrate::Reason::PathQueue);
        assert_eq!(change.bitrate(), 1_200_000);
        assert_eq!(rate.applied_bitrate(), INITIAL);
        let capture = rate.begin_capture().unwrap();
        assert_eq!(capture.bitrate(), 1_200_000);
        rate.complete_capture(capture, 1_200_000, true, 200_000)
            .unwrap();
        assert_eq!(rate.applied_bitrate(), 1_200_000);
        assert_eq!(rate.ceiling_bitrate(), INITIAL);
        assert_eq!(rate.minimum_bitrate(), INITIAL / 8);
    }
}

#[test]
fn every_profile_retains_immediate_source_credit_and_receiver_overload_response() {
    for profile in PROFILES {
        for (cause, reason) in [
            (0, pacing::Reason::SourceWork),
            (1, pacing::Reason::CaptureCredit),
            (2, pacing::Reason::ReceiverBacklog),
            (3, pacing::Reason::DecoderWork),
        ] {
            let (mut pacing, mut rate) = controllers(profile);
            for now in [0, 50_000, 100_000] {
                let mut input = sample(now, A::Ready);
                let mut receiver = ReceiverEvidence::Unobserved;
                match cause {
                    0 => input.source_work_us = Some(500_000),
                    1 => input.capture_credit = A::Blocked,
                    2 => {
                        receiver = ReceiverEvidence::Measured(Load {
                            retained_bytes: 2_048,
                            retained_pictures: 2,
                            decoding: false,
                            work_us: Some(5_000),
                        });
                    }
                    _ => {
                        receiver = ReceiverEvidence::Measured(Load {
                            retained_bytes: 1_024,
                            retained_pictures: 1,
                            decoding: true,
                            work_us: Some(500_000),
                        });
                    }
                }
                let report = pacing.update_with_receiver(input, receiver).unwrap();
                rate.observe(&report).unwrap();
                if now < 100_000 {
                    assert_eq!(report.reason, pacing::Reason::Hold);
                } else {
                    assert_eq!(report.reason, reason, "{profile:?}, cause {cause}");
                    assert_eq!(report.interval_us, 80_000);
                }
                // Encoder/decoder workload is not link-capacity evidence.
                assert_eq!(rate.pending(), None);
                assert_eq!(rate.applied_bitrate(), INITIAL);
            }
        }
    }
}

#[test]
fn motion_send_grace_cannot_mask_simultaneous_capture_or_decoder_pressure() {
    for receiver_busy in [false, true] {
        let (mut pacing, _) = controllers(WorkstationProfile::Motion);
        let receiver = if receiver_busy {
            ReceiverEvidence::Measured(Load {
                retained_bytes: 2_048,
                retained_pictures: 2,
                decoding: false,
                work_us: Some(5_000),
            })
        } else {
            ReceiverEvidence::Unobserved
        };
        for now in [0, 50_000, 100_000] {
            let mut input = sample(now, A::Blocked);
            if !receiver_busy {
                input.capture_credit = A::Blocked;
            }
            let report = pacing.update_with_receiver(input, receiver).unwrap();
            if now == 100_000 {
                assert_eq!(
                    report.reason,
                    if receiver_busy {
                        pacing::Reason::ReceiverBacklog
                    } else {
                        pacing::Reason::CaptureCredit
                    }
                );
                assert_eq!(report.interval_us, 80_000);
            }
        }
    }
}

#[test]
fn service_gaps_do_not_contribute_time_to_either_preference_window() {
    for (profile, cadence_at, bitrate_at) in [
        (WorkstationProfile::Auto, 400_000, 400_000),
        (WorkstationProfile::Text, 400_000, 550_000),
        (WorkstationProfile::Motion, 550_000, 400_000),
    ] {
        let (mut pacing, mut rate) = controllers(profile);
        for now in [0, 50_000] {
            observe(&mut pacing, &mut rate, now, A::Blocked);
        }
        let mut first_cadence = None;
        let mut first_rate = None;
        for now in (300_000..=550_000).step_by(50_000) {
            let report = observe(&mut pacing, &mut rate, now, A::Blocked);
            if now == 300_000 {
                assert_eq!(report.reason, pacing::Reason::EvidenceGap);
                assert_eq!(report.interval_us, 40_000);
                assert_eq!(rate.pending(), None);
            }
            if report.reason == pacing::Reason::SendAdmission {
                first_cadence.get_or_insert(now);
            }
            if rate.pending().is_some() {
                first_rate.get_or_insert(now);
            }
        }
        assert_eq!(first_cadence, Some(cadence_at), "{profile:?}");
        assert_eq!(first_rate, Some(bitrate_at), "{profile:?}");
    }
}

#[test]
fn profile_preference_does_not_apply_deferred_targets_or_bypass_the_idr_receipt() {
    for profile in PROFILES {
        let (mut pacing, mut rate) = controllers(profile);
        for now in (0..=250_000).step_by(50_000) {
            observe(&mut pacing, &mut rate, now, A::Blocked);
        }
        let original = rate.pending().unwrap();
        for now in [300_000, 350_000] {
            observe(&mut pacing, &mut rate, now, A::Blocked);
            let capture = rate.begin_capture().unwrap();
            assert_eq!(capture.bitrate(), INITIAL / 2);
            rate.complete_capture(capture, INITIAL, false, now).unwrap();
            assert_eq!(rate.pending(), Some(original));
            assert_eq!(rate.applied_bitrate(), INITIAL);
        }
        let capture = rate.begin_capture().unwrap();
        assert_eq!(
            rate.complete_capture(capture, INITIAL / 2, false, 400_000),
            Err(bitrate::Error::MissingIdr)
        );
        assert_eq!(rate.applied_bitrate(), INITIAL);
        assert_eq!(rate.pending(), Some(original));
        rate.complete_capture(capture, INITIAL / 2, true, 400_000)
            .unwrap();
        assert_eq!(rate.pending(), None);
        assert_eq!(rate.applied_bitrate(), INITIAL / 2);
    }
}

#[test]
fn unknown_or_stale_source_evidence_never_restores_quality_in_any_profile() {
    for profile in PROFILES {
        let (mut pacing, mut rate) = controllers(profile);
        for now in (0..=250_000).step_by(50_000) {
            observe(&mut pacing, &mut rate, now, A::Blocked);
        }
        let capture = rate.begin_capture().unwrap();
        rate.complete_capture(capture, INITIAL / 2, true, 250_000)
            .unwrap();
        let interval = pacing.interval_us();
        for now in (300_000..=6_000_000).step_by(50_000) {
            let mut input = sample(now, A::Ready);
            input.observation = None;
            let report = pacing
                .update_with_receiver(input, ReceiverEvidence::Unknown)
                .unwrap();
            rate.observe(&report).unwrap();
            rate.observe_path_evidence(now, PathEvidence::Unknown)
                .unwrap();
            assert_eq!(report.mode, Mode::Active);
            assert_eq!(report.interval_us, interval);
            assert_eq!(report.headroom_us, 0);
            assert_eq!(rate.pending(), None);
            assert_eq!(rate.applied_bitrate(), INITIAL / 2);
        }
    }
}

#[test]
fn automatic_constructors_and_explicit_auto_replay_identical_native_decisions() {
    let mut original_pacing = pacing::Controller::new(policy()).unwrap();
    let mut original_rate = bitrate::Controller::new(INITIAL).unwrap();
    let (mut explicit_pacing, mut explicit_rate) = controllers(WorkstationProfile::Auto);
    for step in 0..600 {
        let now = step * 50_000;
        let phase = step % 100;
        let send = if phase < 12 {
            A::Blocked
        } else if phase < 80 {
            A::Ready
        } else {
            A::Unknown
        };
        let mut input = sample(now, send);
        input.observation.as_mut().unwrap().changed = phase < 20;
        if step % 17 == 0 {
            input.source_work_us = Some(500_000);
        }
        if step % 29 == 0 {
            input.capture_credit = A::Blocked;
        }
        let receiver = if step % 23 == 0 {
            ReceiverEvidence::Unknown
        } else {
            ReceiverEvidence::Unobserved
        };
        let original = original_pacing
            .update_with_receiver(input, receiver)
            .unwrap();
        let explicit = explicit_pacing
            .update_with_receiver(input, receiver)
            .unwrap();
        assert_eq!(original, explicit);
        original_rate.observe(&original).unwrap();
        explicit_rate.observe(&explicit).unwrap();
        let path = if phase < 10 {
            PathEvidence::Queued {
                delivered_bits_per_second: 3_000_000,
            }
        } else if phase < 80 {
            PathEvidence::Clear
        } else {
            PathEvidence::Idle
        };
        original_rate.observe_path_evidence(now, path).unwrap();
        explicit_rate.observe_path_evidence(now, path).unwrap();
        assert_eq!(original_rate.pending(), explicit_rate.pending());
        let original_capture = original_rate.begin_capture().unwrap();
        let explicit_capture = explicit_rate.begin_capture().unwrap();
        assert_eq!(original_capture.bitrate(), explicit_capture.bitrate());
        original_rate
            .complete_capture(original_capture, original_capture.bitrate(), true, now)
            .unwrap();
        explicit_rate
            .complete_capture(explicit_capture, explicit_capture.bitrate(), true, now)
            .unwrap();
        assert_eq!(
            original_rate.applied_bitrate(),
            explicit_rate.applied_bitrate()
        );
    }
}

#[test]
fn every_profile_remains_within_original_rate_and_cadence_bounds_through_long_pressure() {
    for profile in PROFILES {
        for initial in [10_000, 100_000, INITIAL, 200_000_000] {
            let mut pacing = pacing::Controller::new_with_profile(policy(), profile).unwrap();
            let mut rate = bitrate::Controller::new_with_profile(initial, profile).unwrap();
            let mut last_rate_change = None;
            for step in 0..600 {
                let now = step * 50_000;
                let send = if step % 200 < 100 {
                    A::Blocked
                } else {
                    A::Ready
                };
                let report = observe(&mut pacing, &mut rate, now, send);
                assert!((20_000..=200_000).contains(&report.interval_us));
                let capture = rate.begin_capture().unwrap();
                let actual = capture.bitrate();
                assert!((rate.minimum_bitrate()..=initial).contains(&actual));
                if actual != rate.applied_bitrate() {
                    if let Some(previous) = last_rate_change {
                        assert!(now - previous >= 2_000_000);
                    }
                    last_rate_change = Some(now);
                }
                rate.complete_capture(capture, actual, true, now).unwrap();
                assert_eq!(rate.ceiling_bitrate(), initial);
                assert_eq!(rate.profile(), profile);
                assert_eq!(pacing.profile(), profile);
            }
        }
    }
}
