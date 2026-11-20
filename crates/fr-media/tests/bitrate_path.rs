//! `Controller::observe_path`: a held queue on the viewer's path lowers the
//! rate toward what the path delivered; a clear path probes back up (fr-7al9).

use fr_media::bitrate::{Controller, Reason};

const TURN: u64 = 20_000;

/// Samples every turn over `[from, to)`, all queued or all clear.
fn path(c: &mut Controller, from: u64, to: u64, queued: bool, delivered: u64) {
    for now in (from..to).step_by(usize::try_from(TURN).unwrap()) {
        c.observe_path(now, queued, delivered).unwrap();
    }
}

/// Apply the pending change on one changed capture, with its IDR.
fn apply(c: &mut Controller, now: u64) -> u32 {
    let capture = c.begin_capture().unwrap();
    let bitrate = capture.bitrate();
    c.complete_capture(capture, bitrate, true, now).unwrap();
    assert_eq!(c.applied_bitrate(), bitrate);
    assert_eq!(c.pending(), None);
    bitrate
}

#[test]
fn a_held_queue_cuts_to_three_fifths_of_what_the_path_delivered() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 200_000, true, 800_000);
    assert_eq!(c.pending(), None, "held under 200 ms");
    c.observe_path(200_000, true, 800_000).unwrap();
    let cut = c.pending().unwrap();
    assert_eq!(cut.reason(), Reason::PathQueue);
    assert_eq!(cut.bitrate(), 480_000);
    assert_eq!(c.applied_bitrate(), 2_000_000, "a proposal is not a change");
    assert_eq!(apply(&mut c, 200_000), 480_000);
}

#[test]
fn queues_shorter_than_the_pressure_time_never_cut() {
    // A healthy path's bursts: 180 ms queued, 20 ms clear, for 20 s.
    let mut c = Controller::new(2_000_000).unwrap();
    for start in (0..20_000_000).step_by(200_000) {
        path(&mut c, start, start + 180_000, true, 500_000);
        c.observe_path(start + 180_000, false, 500_000).unwrap();
        assert_eq!(c.pending(), None);
    }
    assert_eq!(c.applied_bitrate(), 2_000_000);
}

#[test]
fn an_unmeasured_path_cuts_by_two_fifths_and_stops_at_the_floor() {
    let mut c = Controller::new(2_000_000).unwrap();
    let mut now = 0;
    let mut applied = Vec::new();
    for _ in 0..6 {
        let until = now + 2_000_000 + 2 * TURN;
        while c.pending().is_none() && now < until {
            c.observe_path(now, true, 0).unwrap();
            now += TURN;
        }
        if c.pending().is_some() {
            applied.push(apply(&mut c, now));
        }
    }
    // Three fifths each time, two seconds apart, down to the floor (an
    // eighth of the original rate): 250 kbit/s.
    assert_eq!(applied, [1_200_000, 720_000, 432_000, 259_200, 250_000]);
    assert_eq!(c.minimum_bitrate(), 250_000);
}

#[test]
fn a_cut_waits_for_the_change_dwell_and_uses_the_newest_delivery() {
    let mut c = Controller::new(8_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 5_000_000);
    assert_eq!(
        apply(&mut c, 220_000),
        3_000_000,
        "delivery below 3/5 of the rate"
    );
    // Still queued: nothing within two seconds of the actual change.
    path(&mut c, 220_000, 2_220_000, true, 2_000_000);
    assert_eq!(c.pending(), None);
    c.observe_path(2_220_000, true, 2_000_000).unwrap();
    assert_eq!(c.pending().unwrap().bitrate(), 1_200_000);
}

#[test]
fn delivery_reading_high_still_cuts_by_two_fifths() {
    // Each acknowledged packet counts as full, so a queued path's delivery can
    // read above the rate: it must neither raise the rate nor stop the cut.
    let mut c = Controller::new(1_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 4_000_000);
    let cut = c.pending().unwrap();
    assert_eq!(cut.reason(), Reason::PathQueue);
    assert_eq!(cut.bitrate(), 600_000);
}

#[test]
fn a_clear_path_probes_a_quarter_up_every_four_seconds_to_the_ceiling() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    assert_eq!(apply(&mut c, 220_000), 480_000);
    let mut now = 220_000;
    let mut steps = Vec::new();
    while c.applied_bitrate() < 2_000_000 && now < 120_000_000 {
        c.observe_path(now, false, 0).unwrap();
        if let Some(change) = c.pending() {
            assert_eq!(change.reason(), Reason::Headroom);
            steps.push((now, apply(&mut c, now)));
        }
        now += TURN;
    }
    let rates: Vec<u32> = steps.iter().map(|&(_, rate)| rate).collect();
    assert_eq!(
        rates,
        [
            600_000, 750_000, 937_500, 1_171_875, 1_464_844, 1_831_055, 2_000_000
        ]
    );
    // Four seconds of clear path before the first probe. Later probes require
    // fresh samples after the actual change, beginning on the next turn.
    assert_eq!(steps[0].0, 220_000 + 4_000_000);
    for pair in steps.windows(2) {
        assert_eq!(pair[1].0 - pair[0].0, 4_000_000 + TURN);
    }
}

#[test]
fn a_queue_cancels_an_unsent_probe_and_then_cuts() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    apply(&mut c, 220_000);
    path(&mut c, 220_000, 4_240_000, false, 0);
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
    // The probe was never carried by a capture: the queue simply drops it.
    c.observe_path(4_240_000, true, 700_000).unwrap();
    assert_eq!(c.pending(), None);
    path(&mut c, 4_260_000, 4_460_000, true, 700_000);
    let cut = c.pending().unwrap();
    assert_eq!(cut.reason(), Reason::PathQueue);
    assert_eq!(cut.bitrate(), 288_000);
}

#[test]
fn a_deferred_probe_is_restored_explicitly_when_a_queue_appears() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    apply(&mut c, 220_000);
    path(&mut c, 220_000, 4_240_000, false, 0);
    // An unchanged screen deferred the probe's target on the native side.
    let deferred = c.begin_capture().unwrap();
    assert_eq!(deferred.bitrate(), 600_000);
    c.complete_capture(deferred, 480_000, false, 4_240_000)
        .unwrap();
    c.observe_path(4_260_000, true, 700_000).unwrap();
    let restore = c.pending().unwrap();
    assert_eq!(restore.reason(), Reason::EvidenceReset);
    assert_eq!(restore.bitrate(), 480_000);
}

#[test]
fn replacing_the_cohort_erases_its_queued_and_clear_path_intervals() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 180_000, true, 800_000);
    c.reset_evidence().unwrap();
    path(&mut c, 180_000, 380_000, true, 800_000);
    assert_eq!(c.pending(), None, "the new cohort needs its own pressure");
    c.observe_path(380_000, true, 800_000).unwrap();
    assert_eq!(apply(&mut c, 380_000), 480_000);

    path(&mut c, 380_000, 4_360_000, false, 0);
    c.reset_evidence().unwrap();
    path(&mut c, 4_360_000, 8_360_000, false, 0);
    assert_eq!(c.pending(), None, "the old cohort cannot donate headroom");
    c.observe_path(8_360_000, false, 0).unwrap();
    assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
    assert_eq!(c.pending().unwrap().bitrate(), 600_000);
    assert_eq!(c.ceiling_bitrate(), 2_000_000);
    assert_eq!(c.minimum_bitrate(), 250_000);
}

#[test]
fn applying_a_native_change_requires_new_path_headroom_at_the_new_rate() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    let cut = c.pending();
    // A native request can be deferred while the path clears. This was all
    // measured at the old rate, so it cannot support probing the new rate.
    path(&mut c, 220_000, 5_220_000, false, 0);
    assert_eq!(c.pending(), cut);
    assert_eq!(apply(&mut c, 5_220_000), 480_000);
    path(&mut c, 5_220_000, 9_220_000, false, 0);
    assert_eq!(c.pending(), None);
    c.observe_path(9_220_000, false, 0).unwrap();
    assert_eq!(c.pending().unwrap().bitrate(), 600_000);
}

#[test]
fn path_pressure_has_an_exact_bounded_sampling_gap() {
    for gap in [100_000, 100_001] {
        let mut c = Controller::new(2_000_000).unwrap();
        c.observe_path(0, true, 800_000).unwrap();
        // A native completion advances the policy clock, not path evidence.
        let capture = c.begin_capture().unwrap();
        c.complete_capture(capture, 2_000_000, false, gap - 1)
            .unwrap();
        c.observe_path(gap, true, 800_000).unwrap();
        assert_eq!(c.pending(), None);
        c.observe_path(gap + 100_000, true, 800_000).unwrap();
        if gap > 100_000 {
            assert_eq!(c.pending(), None, "the unobserved interval is not pressure");
            c.observe_path(gap + 200_000, true, 800_000).unwrap();
        }
        assert_eq!(c.pending().unwrap().bitrate(), 480_000);
    }
}

#[test]
fn a_long_sampling_gap_cannot_turn_one_clear_sample_into_four_seconds_of_headroom() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    apply(&mut c, 220_000);
    c.observe_path(220_000, false, 0).unwrap();
    // Successful no-change completions are not intervening path samples.
    for now in [2_220_000, 4_220_000] {
        let capture = c.begin_capture().unwrap();
        c.complete_capture(capture, 480_000, false, now).unwrap();
    }
    c.observe_path(4_220_001, false, 0).unwrap();
    assert_eq!(c.pending(), None);
    path(&mut c, 4_240_001, 8_220_001, false, 0);
    assert_eq!(c.pending(), None);
    c.observe_path(8_220_001, false, 0).unwrap();
    assert_eq!(c.pending().unwrap().bitrate(), 600_000);
}

#[test]
fn a_path_gap_cancels_a_probe_without_losing_deferred_or_in_flight_native_effects() {
    for native in 0..3 {
        let mut c = Controller::new(2_000_000).unwrap();
        path(&mut c, 0, 220_000, true, 800_000);
        apply(&mut c, 220_000);
        path(&mut c, 220_000, 4_240_000, false, 0);
        let probe = c.pending().unwrap();
        assert_eq!(probe.bitrate(), 600_000);
        let capture = (native > 0).then(|| c.begin_capture().unwrap());
        if native == 1 {
            c.complete_capture(capture.unwrap(), 480_000, false, 4_240_000)
                .unwrap();
        }
        c.observe_path(4_320_001, false, 0).unwrap();
        if native == 0 {
            assert_eq!(c.pending(), None);
            assert_eq!(c.begin_capture().unwrap().bitrate(), 480_000);
            continue;
        }
        let restore = c.pending().unwrap();
        assert_ne!(restore.id(), probe.id());
        assert_eq!(restore.reason(), Reason::EvidenceReset);
        assert_eq!(restore.bitrate(), 480_000);
        if native == 2 {
            // The original request was already running: its actual IDR still
            // changes the applied rate despite the later evidence reset.
            c.complete_capture(capture.unwrap(), 600_000, true, 4_340_000)
                .unwrap();
            assert_eq!(c.applied_bitrate(), 600_000);
            assert_eq!(c.pending(), Some(restore));
        }
        c.reset_evidence().unwrap();
        assert_eq!(c.pending(), Some(restore));
        let cancellation = c.begin_capture().unwrap();
        assert_eq!(cancellation.bitrate(), 480_000);
        let previous = c.applied_bitrate();
        c.complete_capture(cancellation, previous, false, 4_360_000)
            .unwrap();
        if native == 2 {
            assert_eq!(
                c.pending(),
                Some(restore),
                "native dwell can defer restoration"
            );
            let cancellation = c.begin_capture().unwrap();
            c.complete_capture(cancellation, 480_000, true, 6_340_000)
                .unwrap();
        }
        assert_eq!(c.applied_bitrate(), 480_000);
        assert_eq!(c.pending(), None);
    }
}

#[test]
fn a_path_gap_preserves_an_already_justified_conservative_cut() {
    let mut c = Controller::new(2_000_000).unwrap();
    path(&mut c, 0, 220_000, true, 800_000);
    let cut = c.pending();
    c.observe_path(1_000_000, false, 0).unwrap();
    assert_eq!(c.pending(), cut);
    assert_eq!(apply(&mut c, 1_000_000), 480_000);
}

#[test]
fn path_time_cannot_run_backwards() {
    let mut c = Controller::new(2_000_000).unwrap();
    c.observe_path(100_000, false, 0).unwrap();
    assert!(c.observe_path(99_999, false, 0).is_err());
}
