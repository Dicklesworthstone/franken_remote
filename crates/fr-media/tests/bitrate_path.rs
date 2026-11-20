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
    // Four seconds of clear path before the first probe and between probes.
    assert_eq!(steps[0].0, 220_000 + 4_000_000);
    for pair in steps.windows(2) {
        assert_eq!(pair[1].0 - pair[0].0, 4_000_000);
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
fn path_time_cannot_run_backwards() {
    let mut c = Controller::new(2_000_000).unwrap();
    c.observe_path(100_000, false, 0).unwrap();
    assert!(c.observe_path(99_999, false, 0).is_err());
}
