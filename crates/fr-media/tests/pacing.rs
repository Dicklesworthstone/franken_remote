use fr_media::pacing::{
    Availability as A, Controller, Error, Mode, Observation, Policy, Reason, Sample,
};
fn controller() -> Controller {
    Controller::new(Policy {
        minimum_interval_us: 20_000,
        maximum_interval_us: 200_000,
    })
    .unwrap()
}
fn active(now_us: u64) -> Sample {
    Sample {
        now_us,
        source_work_us: Some(5_000),
        send: A::Ready,
        capture_credit: A::Ready,
        observation: Some(Observation {
            at_us: now_us,
            changed: true,
        }),
    }
}
#[test]
fn overload_needs_continuous_evidence_and_changes_are_bounded_and_attributed() {
    for (kind, reason) in [
        (0, Reason::SourceWork),
        (1, Reason::SendAdmission),
        (2, Reason::CaptureCredit),
    ] {
        let mut c = controller();
        for i in 0..=20 {
            let mut s = active(i * 50_000);
            match kind {
                0 => s.source_work_us = Some(1_000_000),
                1 => s.send = A::Blocked,
                _ => s.capture_credit = A::Blocked,
            }
            let r = c.update(s).unwrap();
            if i < 2 {
                assert_eq!(r.reason, Reason::Hold);
            }
            if r.reason != Reason::Hold {
                assert_eq!(r.reason, reason);
                assert!(r.pressure_us[kind] >= 100_000);
                assert_eq!(r.interval_us, (r.previous_interval_us * 2).min(200_000));
            }
        }
        assert_eq!(c.interval_us(), 200_000);
        let decisions: Vec<_> = c.decisions().collect();
        assert_eq!(decisions.len(), 3);
        for pair in decisions.windows(2) {
            assert!(pair[1].sample.now_us - pair[0].sample.now_us >= 250_000);
        }
    }
}
#[test]
fn brief_pressure_and_unknown_gaps_do_not_accumulate_into_an_overload() {
    let mut c = controller();
    for i in 0..200 {
        let mut s = active(i * 25_000);
        s.send = if i % 4 < 3 { A::Blocked } else { A::Unknown };
        assert_eq!(c.update(s).unwrap().reason, Reason::Hold);
    }
    assert_eq!(c.interval_us(), 40_000);
}
#[test]
fn probes_are_small_slow_and_never_exceed_configured_frame_rate() {
    let mut c = controller();
    for i in 0..=300 {
        let r = c.update(active(i * 50_000)).unwrap();
        assert!(r.interval_us >= 20_000);
        if i < 40 {
            assert_eq!(r.interval_us, 40_000);
        }
        if r.reason == Reason::HeadroomProbe {
            assert!(r.headroom_us >= 2_000_000);
            assert_eq!(
                r.interval_us,
                (r.previous_interval_us - r.previous_interval_us.div_ceil(8)).max(20_000)
            );
        }
    }
    assert_eq!(c.interval_us(), 20_000);
    for pair in c.decisions().collect::<Vec<_>>().windows(2) {
        assert!(pair[1].sample.now_us - pair[0].sample.now_us >= 2_000_000);
    }
}
#[test]
fn unavailable_telemetry_never_claims_headroom() {
    for unknown in 0..3 {
        let mut c = controller();
        for i in 0..=100 {
            let mut s = active(i * 50_000);
            match unknown {
                0 => s.source_work_us = None,
                1 => s.send = A::Unknown,
                _ => s.capture_credit = A::Unknown,
            }
            assert_eq!(c.update(s).unwrap().reason, Reason::Hold);
        }
        assert_eq!(c.interval_us(), 40_000);
    }
}
#[test]
fn no_packets_and_duplicate_source_metadata_are_not_verified_idle() {
    let mut c = controller();
    for i in 0..=100 {
        let mut s = active(i * 50_000);
        s.observation = Some(Observation {
            at_us: 0,
            changed: false,
        });
        let r = c.update(s).unwrap();
        assert_eq!(r.mode, Mode::Active);
        assert_eq!(r.interval_us, 40_000);
    }
    let mut empty = controller();
    for i in 0..100 {
        let mut s = active(i * 50_000);
        s.observation = None;
        assert_eq!(empty.update(s).unwrap().mode, Mode::Active);
    }
}
#[test]
fn verified_static_source_slows_capture_and_change_wakes_conservatively() {
    let mut c = controller();
    for i in 0..=30 {
        let mut s = active(i * 50_000);
        s.observation.as_mut().unwrap().changed = false;
        let r = c.update(s).unwrap();
        if i < 20 {
            assert_eq!(r.mode, Mode::Active);
        }
        if i == 20 {
            assert_eq!(r.reason, Reason::VerifiedIdle);
            assert_eq!(r.interval_us, 200_000);
        }
    }
    let r = c.update(active(1_550_000)).unwrap();
    assert_eq!(r.reason, Reason::ChangedAfterIdle);
    assert_eq!(r.mode, Mode::Active);
    assert_eq!(r.interval_us, 40_000);
    assert_eq!(c.update(active(1_600_000)).unwrap().reason, Reason::Hold);
}
#[test]
fn change_after_idle_never_overrides_observed_backpressure() {
    let mut c = controller();
    for i in 0..=20 {
        let mut s = active(i * 50_000);
        s.observation.as_mut().unwrap().changed = false;
        c.update(s).unwrap();
    }
    let mut wake = active(1_050_000);
    wake.send = A::Blocked;
    let r = c.update(wake).unwrap();
    assert_eq!(r.reason, Reason::ChangedAfterIdle);
    assert_eq!(r.mode, Mode::Active);
    assert_eq!(r.interval_us, 200_000);
}
#[test]
fn missing_source_evidence_ends_idle_classification_without_inventing_a_change() {
    let mut c = controller();
    for i in 0..=20 {
        let mut s = active(i * 50_000);
        s.observation.as_mut().unwrap().changed = false;
        c.update(s).unwrap();
    }
    for i in 21..=26 {
        let mut s = active(i * 50_000);
        s.observation = None;
        c.update(s).unwrap();
    }
    let r = c.report().unwrap();
    assert_eq!(r.mode, Mode::Active);
    assert_eq!(r.reason, Reason::EvidenceGap);
    assert_eq!(r.interval_us, 200_000);
}
#[test]
fn scheduling_gap_resets_estimates_and_requires_new_continuous_evidence() {
    let mut c = controller();
    for i in 0..=40 {
        c.update(active(i * 50_000)).unwrap();
    }
    assert_eq!(c.interval_us(), 35_000);
    let mut stalled = active(5_000_000);
    stalled.send = A::Blocked;
    let r = c.update(stalled).unwrap();
    assert_eq!(r.reason, Reason::EvidenceGap);
    assert_eq!(r.interval_us, 40_000);
    assert_eq!(r.pressure_us, [0; 3]);
    assert_eq!(r.headroom_us, 0);
}
#[test]
fn timestamp_faults_are_terminal_and_large_valid_clocks_do_not_overflow() {
    let mut c = controller();
    c.update(active(100)).unwrap();
    assert_eq!(c.update(active(99)), Err(Error::ClockRegression));
    assert_eq!(c.update(active(101)), Err(Error::Closed));
    for at in [99, 102] {
        let mut c = controller();
        c.update(active(100)).unwrap();
        let mut s = active(101);
        s.observation.as_mut().unwrap().at_us = at;
        assert_eq!(c.update(s), Err(Error::InvalidObservation));
    }
    let mut c = controller();
    c.update(active(u64::MAX - 50_000)).unwrap();
    c.update(active(u64::MAX)).unwrap();
}
#[test]
fn policy_refuses_bad_bounds_without_saturating_or_wrapping() {
    for (minimum_interval_us, maximum_interval_us) in [
        (0, 100_000),
        (999, 100_000),
        (20_000, 19_999),
        (20_000, 200_001),
        (u64::MAX, u64::MAX),
    ] {
        assert!(matches!(
            Controller::new(Policy {
                minimum_interval_us,
                maximum_interval_us
            }),
            Err(Error::InvalidPolicy)
        ));
    }
    let mut c = Controller::new(Policy {
        minimum_interval_us: 200_000,
        maximum_interval_us: 200_000,
    })
    .unwrap();
    c.update(active(0)).unwrap();
    assert_eq!(c.interval_us(), 200_000);
}
#[test]
fn trace_replay_is_exact_and_adjustment_retention_is_fixed() {
    let mut a = controller();
    let mut b = controller();
    for i in 0..4000 {
        let mut s = active(i * 50_000);
        if i % 200 < 10 {
            s.send = A::Blocked;
        }
        assert_eq!(a.update(s), b.update(s));
    }
    assert_eq!(a.decisions().count(), 16);
    assert_eq!(
        a.decisions().collect::<Vec<_>>(),
        b.decisions().collect::<Vec<_>>()
    );
    assert!(core::mem::size_of::<Controller>() < 8192);
}

#[test]
fn brief_unknown_capture_credit_pauses_but_never_counts_as_headroom() {
    let mut c = controller();
    let initial = c.interval_us();
    let mut first_probe = None;
    for tick in 0..=300 {
        let mut s = active(tick * 10_000);
        // One outstanding native operation every 100 ms. Unknown credit is
        // not spare credit, but the other 80 ms have measured ready endpoints.
        if tick % 10 == 0 {
            s.capture_credit = A::Unknown;
        }
        let r = c.update(s).unwrap();
        if tick % 10 == 0 {
            assert_eq!(r.headroom_us, 0);
        }
        if r.reason == Reason::HeadroomProbe && first_probe.is_none() {
            first_probe = Some(tick * 10_000);
        }
        if tick <= 200 {
            assert_eq!(r.interval_us, initial);
        }
    }
    let probe = first_probe.expect("brief genuine work must not disable recovery forever");
    assert_eq!(probe, 2_490_000);
}

#[test]
fn extended_unknown_or_known_pressure_cannot_bank_headroom_for_a_later_probe() {
    for blocked in [false, true] {
        let mut c = controller();
        for i in 0..=38 {
            c.update(active(i * 50_000)).unwrap();
        }
        for i in 39..=44 {
            let mut s = active(i * 50_000);
            s.capture_credit = if blocked { A::Blocked } else { A::Unknown };
            let r = c.update(s).unwrap();
            assert_ne!(r.reason, Reason::HeadroomProbe);
        }
        for i in 45..=64 {
            let r = c.update(active(i * 50_000)).unwrap();
            assert_ne!(r.reason, Reason::HeadroomProbe);
            assert!(r.headroom_us < 2_000_000);
        }
    }
}

#[test]
fn explicitly_slow_fixed_policies_stay_fixed_without_widening_adaptation() {
    for interval in [250_000, 500_000, 1_000_000] {
        let mut c = Controller::new(Policy {
            minimum_interval_us: interval,
            maximum_interval_us: interval,
        })
        .unwrap();
        for i in 0..=100 {
            let mut s = active(i * 50_000);
            s.capture_credit = A::Blocked;
            assert_eq!(c.update(s).unwrap().interval_us, interval);
        }
    }
    assert!(
        Controller::new(Policy {
            minimum_interval_us: 200_000,
            maximum_interval_us: 250_000
        })
        .is_err()
    );
    assert!(
        Controller::new(Policy {
            minimum_interval_us: 1_000_001,
            maximum_interval_us: 1_000_001
        })
        .is_err()
    );
}
