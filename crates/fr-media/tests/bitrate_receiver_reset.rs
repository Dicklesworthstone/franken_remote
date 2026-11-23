//! A decoder restart on the same transport may retain a proven conservative
//! path cut. Receiver/local pressure and optimistic probes need new evidence.
use fr_media::{
    bitrate::{Controller, Reason},
    pacing::{Availability, Controller as Pacer, Observation, Policy, Sample},
};

fn queued(c: &mut Controller, from: u64, to: u64) {
    for now in (from..=to).step_by(20_000) {
        c.observe_path(now, true, 800_000).unwrap();
    }
}

fn path_cut() -> Controller {
    let mut c = Controller::new(2_000_000).unwrap();
    queued(&mut c, 0, 200_000);
    assert_eq!(c.pending().unwrap().reason(), Reason::PathQueue);
    c
}

#[test]
fn receiver_reset_preserves_same_path_pressure_and_its_original_native_cut_token() {
    for submitted in [false, true] {
        let mut c = Controller::new(2_000_000).unwrap();
        queued(&mut c, 0, 100_000);
        c.reset_receiver_evidence().unwrap();
        queued(&mut c, 120_000, 200_000);
        let cut = c.pending().expect("same path keeps its first 100 ms");
        assert_eq!(cut.reason(), Reason::PathQueue);
        if submitted {
            let deferred = c.begin_capture().unwrap();
            c.complete_capture(deferred, 2_000_000, false, 220_000)
                .unwrap();
        }
        c.reset_receiver_evidence().unwrap();
        assert_eq!(c.pending(), Some(cut));
        let in_flight = c.begin_capture().unwrap();
        c.reset_receiver_evidence().unwrap();
        assert_eq!(c.pending(), Some(cut));
        assert_eq!(c.applied_bitrate(), 2_000_000);
        c.complete_capture(in_flight, cut.bitrate(), true, 240_000)
            .unwrap();
        assert_eq!(c.applied_bitrate(), cut.bitrate());
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn a_real_path_cohort_reset_still_cancels_recovery_intent_and_records_in_flight_effects() {
    for submitted in [false, true] {
        let mut c = path_cut();
        c.reset_receiver_evidence().unwrap();
        let original = submitted.then(|| c.begin_capture().unwrap());
        c.reset_evidence().unwrap();
        if let Some(original) = original {
            let restore = c.pending().unwrap();
            assert_eq!(restore.reason(), Reason::EvidenceReset);
            assert_eq!(restore.bitrate(), 2_000_000);
            c.reset_receiver_evidence().unwrap();
            c.complete_capture(original, original.bitrate(), true, 220_000)
                .unwrap();
            assert_eq!(c.applied_bitrate(), original.bitrate());
            assert_eq!(c.pending(), Some(restore));
            let next = c.begin_capture().unwrap();
            assert_eq!(next.bitrate(), 2_000_000);
            c.complete_capture(next, next.bitrate(), true, 2_220_000)
                .unwrap();
        } else {
            assert_eq!(c.pending(), None);
        }
        assert_eq!(c.applied_bitrate(), 2_000_000);
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn receiver_reset_cancels_unsubmitted_and_native_deferred_local_pressure() {
    for submitted in [false, true] {
        let mut c = Controller::new(2_000_000).unwrap();
        let mut p = Pacer::new(Policy {
            minimum_interval_us: 40_000,
            maximum_interval_us: 40_000,
        })
        .unwrap();
        for now in (0..=100_000).step_by(20_000) {
            let report = p
                .update(Sample {
                    now_us: now,
                    source_work_us: Some(5_000),
                    send: Availability::Blocked,
                    capture_credit: Availability::Ready,
                    observation: Some(Observation {
                        at_us: now,
                        changed: true,
                    }),
                })
                .unwrap();
            c.observe(&report).unwrap();
        }
        assert_eq!(c.pending().unwrap().reason(), Reason::SendPressure);
        if submitted {
            let deferred = c.begin_capture().unwrap();
            c.complete_capture(deferred, 2_000_000, false, 120_000)
                .unwrap();
        }
        c.reset_receiver_evidence().unwrap();
        if submitted {
            assert_eq!(c.pending().unwrap().reason(), Reason::EvidenceReset);
        } else {
            assert_eq!(c.pending(), None);
        }
        let next = c.begin_capture().unwrap();
        assert_eq!(next.bitrate(), 2_000_000);
        c.complete_capture(next, next.bitrate(), false, 140_000)
            .unwrap();
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn receiver_reset_cancels_probes_without_erasing_an_already_running_native_effect() {
    for submission in 0..3 {
        let mut c = path_cut();
        let cut = c.begin_capture().unwrap();
        c.complete_capture(cut, cut.bitrate(), true, 200_000)
            .unwrap();
        for now in (220_000..=4_220_000).step_by(20_000) {
            c.observe_path(now, false, 0).unwrap();
        }
        assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
        let probe = (submission != 0).then(|| c.begin_capture().unwrap());
        if submission == 1 {
            c.complete_capture(probe.unwrap(), cut.bitrate(), false, 4_240_000)
                .unwrap();
        }
        c.reset_receiver_evidence().unwrap();
        if let Some(probe) = probe {
            let restore = c.pending().unwrap();
            assert_eq!(restore.reason(), Reason::EvidenceReset);
            assert_eq!(restore.bitrate(), cut.bitrate());
            if submission == 2 {
                c.complete_capture(probe, probe.bitrate(), true, 4_240_000)
                    .unwrap();
                assert_eq!(c.applied_bitrate(), probe.bitrate());
                assert_eq!(c.pending(), Some(restore));
            }
        } else {
            assert_eq!(c.pending(), None);
        }
        let next = c.begin_capture().unwrap();
        assert_eq!(next.bitrate(), cut.bitrate());
        c.complete_capture(next, next.bitrate(), true, 6_240_000)
            .unwrap();
        assert_eq!(c.applied_bitrate(), cut.bitrate());
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn receiver_reset_requires_a_complete_new_probe_interval_on_the_same_path() {
    let mut c = path_cut();
    let cut = c.begin_capture().unwrap();
    c.complete_capture(cut, cut.bitrate(), true, 200_000)
        .unwrap();
    for now in (220_000..=4_200_000).step_by(20_000) {
        c.observe_path(now, false, 0).unwrap();
    }
    assert_eq!(c.pending(), None);
    c.reset_receiver_evidence().unwrap();
    for now in (4_220_000..8_220_000).step_by(20_000) {
        c.observe_path(now, false, 0).unwrap();
        assert_eq!(c.pending(), None);
    }
    c.observe_path(8_220_000, false, 0).unwrap();
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
}
