//! Actual portable `VideoToolbox` output owner and dual-clock rate policy.
//! Native callback outcomes are supplied here; this is not hardware evidence.
#![forbid(unsafe_code)]
#[path = "../src/capture/rate.rs"]
mod capture_rate;
#[path = "../src/macos_capture/output.rs"]
mod output;

use capture_rate::Rate;
use fr_media::access_unit::FrameId;
use output::Output;
use std::time::{Duration, Instant};

fn established() -> Output {
    let mut output = Output::new(8_000_000);
    assert!(output.force_idr(false));
    output.encoded(FrameId::from_raw(1), 8_000_000);
    output
}

#[test]
fn completed_drop_preserves_reference_and_rate_until_next_output() {
    let mut output = established();
    for _ in 0..20 {
        output
            .skipped(8_000_000, 8_000_000, 0, false, false)
            .unwrap();
        assert_eq!(output.reference(), Some(FrameId::from_raw(1)));
        assert_eq!(output.bitrate(), 8_000_000);
        assert!(!output.force_idr(false));
    }
    output.encoded(FrameId::from_raw(22), 8_000_000);
    assert_eq!(output.reference(), Some(FrameId::from_raw(22)));
}

#[test]
fn native_rate_change_and_cancellation_keep_the_original_physical_dwell() {
    let mut output = established();
    let mut rate = Rate::new(output.bitrate());
    let now = Instant::now();
    rate.request(4_000_000);
    let target = rate.due(10, now, true).unwrap();
    output.skipped(target, target, 0, false, false).unwrap();
    rate.dropped(target, target, 10, now).unwrap();
    assert!(output.force_idr(false));
    assert_eq!(rate.applied_bitrate(), 8_000_000);
    assert_eq!(rate.installed_bitrate(), 4_000_000);
    assert_eq!(output.bitrate(), 8_000_000);
    assert_eq!(output.reference(), Some(FrameId::from_raw(1)));

    // Cancellation cannot undo the actual property mutation's dwell. Further
    // completed drops retain 4 Mbit/s and the same unproduced IDR requirement.
    rate.request(output.bitrate());
    for offset in [1, 100_000, 1_800_000] {
        let worker = now + Duration::from_micros(offset);
        assert_eq!(rate.due(10 + offset, worker, true), None);
        let target = rate.installed_bitrate();
        output.skipped(target, target, 0, false, false).unwrap();
        rate.dropped(target, target, 10 + offset, worker).unwrap();
        assert!(output.force_idr(false));
    }
    // Readback of the prior installed setting is valid while restoration is
    // deferred, even though neither output bitrate nor requested target is 4M.
    output
        .skipped(8_000_000, 4_000_000, 0, false, false)
        .unwrap();
    rate.confirm(
        4_000_000,
        4_000_000,
        true,
        1_900_010,
        now + Duration::from_millis(1_900),
    )
    .unwrap();
    output.encoded(FrameId::from_raw(5), 4_000_000);
    assert!(!output.force_idr(false));
    assert_eq!(
        rate.due(2_000_009, now + Duration::from_secs(2), true),
        None
    );
    assert_eq!(
        rate.due(2_000_010, now + Duration::from_micros(1_999_999), true),
        None
    );
    assert_eq!(
        rate.due(2_000_010, now + Duration::from_secs(2), true),
        Some(8_000_000)
    );
}

#[test]
fn dropped_rate_change_charges_physical_dwell_once_before_qualified_output() {
    let now = Instant::now();
    let mut output = established();
    let mut rate = Rate::new(output.bitrate());
    rate.request(4_000_000);
    let target = rate.due(10, now, true).unwrap();
    output.skipped(target, target, 0, false, false).unwrap();
    rate.dropped(target, target, 10, now).unwrap();
    for host in [20, 40] {
        assert_eq!(rate.due(host, now, true), None);
        output.skipped(target, target, 0, false, false).unwrap();
        rate.dropped(target, target, host, now).unwrap();
        assert!(output.force_idr(false));
        assert_eq!(rate.applied_bitrate(), 8_000_000);
    }
    assert!(rate.confirm(4_000_000, 4_000_000, false, 50, now).is_err());
    rate.confirm(4_000_000, 4_000_000, true, 50, now).unwrap();
    output.encoded(FrameId::from_raw(5), 4_000_000);
    assert_eq!(output.bitrate(), 4_000_000);
    assert!(!output.force_idr(false));
    rate.request(2_000_000);
    assert_eq!(
        rate.due(2_000_009, now + Duration::from_secs(2), true),
        None
    );
    assert_eq!(
        rate.due(2_000_010, now + Duration::from_secs(2), true),
        Some(2_000_000)
    );
}

#[test]
fn invalid_drop_metadata_cannot_mutate_the_published_state() {
    let mut first = Output::new(8_000_000);
    assert!(
        first
            .skipped(8_000_000, 8_000_000, 0, false, false)
            .is_err()
    );
    assert_eq!(first.reference(), None);
    let mut output = established();
    for (actual, length, idr) in [
        (2_000_000, 0, false),
        (4_000_000, 1, false),
        (4_000_000, 0, true),
    ] {
        assert!(
            output
                .skipped(4_000_000, actual, length, idr, false)
                .is_err()
        );
        assert_eq!(output.reference(), Some(FrameId::from_raw(1)));
        assert_eq!(output.bitrate(), 8_000_000);
        assert!(!output.force_idr(false));
    }
}

#[test]
fn original_controller_accepts_installed_output_while_native_restoration_is_deferred() {
    use fr_media::{bitrate, pacing, worker::SkippedCapture};
    let mut host = bitrate::Controller::new(8_000_000).unwrap();
    let mut pacing = pacing::Controller::new(pacing::Policy {
        minimum_interval_us: 40_000,
        maximum_interval_us: 40_000,
    })
    .unwrap();
    for at in [0, 50_000, 100_000] {
        let report = pacing
            .update(pacing::Sample {
                now_us: at,
                source_work_us: Some(5_000),
                send: pacing::Availability::Blocked,
                capture_credit: pacing::Availability::Ready,
                observation: Some(pacing::Observation {
                    at_us: at,
                    changed: true,
                }),
            })
            .unwrap();
        host.observe(&report).unwrap();
    }
    let mut output = established();
    let mut native = Rate::new(output.bitrate());
    let now = Instant::now();
    let cut = host.begin_capture().unwrap();
    native.request(cut.bitrate());
    let target = native.due(100_000, now, true).unwrap();
    assert_eq!(target, 4_000_000);
    output.skipped(target, target, 0, false, false).unwrap();
    native.dropped(target, target, 100_000, now).unwrap();
    let receipt = SkippedCapture::decode(
        &SkippedCapture {
            candidate: FrameId::from_raw(2),
            installed_bitrate: target,
        }
        .encode(),
    )
    .unwrap();
    host.skip_capture(cut, receipt.installed_bitrate, 150_000)
        .unwrap();
    assert_eq!(host.applied_bitrate(), 8_000_000);

    host.reset_evidence().unwrap();
    let restore = host.begin_capture().unwrap();
    assert_eq!(restore.bitrate(), 8_000_000);
    native.request(restore.bitrate());
    assert_eq!(
        native.due(200_000, now + Duration::from_millis(100), true),
        None
    );
    let target = native.installed_bitrate();
    assert!(output.force_idr(false));
    native
        .confirm(
            target,
            target,
            true,
            200_000,
            now + Duration::from_millis(100),
        )
        .unwrap();
    output.encoded(FrameId::from_raw(3), target);
    host.complete_capture(restore, target, true, 200_000)
        .unwrap();
    assert_eq!(host.applied_bitrate(), 4_000_000);
    assert_eq!(host.pending().unwrap().bitrate(), 8_000_000);

    let restore = host.begin_capture().unwrap();
    let target = native
        .due(2_100_000, now + Duration::from_secs(2), true)
        .unwrap();
    native
        .confirm(
            target,
            target,
            true,
            2_100_000,
            now + Duration::from_secs(2),
        )
        .unwrap();
    output.encoded(FrameId::from_raw(4), target);
    host.complete_capture(restore, target, true, 2_100_000)
        .unwrap();
    assert_eq!(host.applied_bitrate(), 8_000_000);
    assert_eq!(output.bitrate(), 8_000_000);
    assert_eq!(host.pending(), None);
}

#[test]
fn a_forced_same_rate_drop_keeps_its_idr_until_real_output() {
    let mut output = established();
    for (actual, length, idr) in [
        (4_000_000, 0, false),
        (8_000_000, 1, false),
        (8_000_000, 0, true),
    ] {
        assert!(
            output
                .skipped(8_000_000, actual, length, idr, true)
                .is_err()
        );
        assert!(
            !output.force_idr(false),
            "invalid replies cannot install a forced boundary"
        );
        assert_eq!(output.reference(), Some(FrameId::from_raw(1)));
        assert_eq!(output.bitrate(), 8_000_000);
    }
    output
        .skipped(8_000_000, 8_000_000, 0, false, true)
        .unwrap();
    assert!(output.force_idr(false));
    output
        .skipped(8_000_000, 8_000_000, 0, false, false)
        .unwrap();
    assert!(
        output.force_idr(false),
        "a later non-forced request cannot forget the IDR"
    );
    output.encoded(FrameId::from_raw(4), 8_000_000);
    assert!(!output.force_idr(false));
}
