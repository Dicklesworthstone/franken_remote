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
        output.skipped(8_000_000, 8_000_000, 0, false).unwrap();
        assert_eq!(output.reference(), Some(FrameId::from_raw(1)));
        assert_eq!(output.bitrate(), 8_000_000);
        assert!(!output.force_idr(false));
    }
    output.encoded(FrameId::from_raw(22), 8_000_000);
    assert_eq!(output.reference(), Some(FrameId::from_raw(22)));
}

#[test]
fn native_rate_change_and_cancellation_keep_an_idr_required_until_real_output() {
    let mut output = established();
    let mut rate = Rate::new(output.bitrate());
    let now = Instant::now();
    rate.request(4_000_000);
    let target = rate.due(10, now, true).unwrap();
    output.skipped(target, target, 0, false).unwrap();
    assert!(output.force_idr(false));
    assert_eq!(rate.applied_bitrate(), 8_000_000);
    assert_eq!(output.bitrate(), 8_000_000);
    assert_eq!(output.reference(), Some(FrameId::from_raw(1)));

    // Cancellation restores the old advertised rate natively, but another
    // drop cannot clear the independent-picture requirement or start dwell.
    rate.request(output.bitrate());
    let target = rate.due(11, now, true).unwrap_or(output.bitrate());
    output.skipped(target, target, 0, false).unwrap();
    assert!(output.force_idr(false));
    rate.confirm(target, target, true, 12, now).unwrap();
    output.encoded(FrameId::from_raw(4), target);
    assert!(!output.force_idr(false));
    assert_eq!(output.reference(), Some(FrameId::from_raw(4)));
    rate.request(4_000_000);
    assert_eq!(rate.due(13, now, true), Some(4_000_000));
}

#[test]
fn dropped_rate_change_spends_no_dwell_and_only_admitted_idr_confirms_it() {
    let now = Instant::now();
    let mut output = established();
    let mut rate = Rate::new(output.bitrate());
    rate.request(4_000_000);
    for host in [10, 20, 40] {
        let target = rate.due(host, now, true).unwrap();
        output.skipped(target, target, 0, false).unwrap();
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
        rate.due(2_000_049, now + Duration::from_secs(2), true),
        None
    );
    assert_eq!(
        rate.due(2_000_050, now + Duration::from_secs(2), true),
        Some(2_000_000)
    );
}

#[test]
fn invalid_drop_metadata_cannot_mutate_the_published_state() {
    let mut first = Output::new(8_000_000);
    assert!(first.skipped(8_000_000, 8_000_000, 0, false).is_err());
    assert_eq!(first.reference(), None);
    let mut output = established();
    for (actual, length, idr) in [
        (2_000_000, 0, false),
        (4_000_000, 1, false),
        (4_000_000, 0, true),
    ] {
        assert!(output.skipped(4_000_000, actual, length, idr).is_err());
        assert_eq!(output.reference(), Some(FrameId::from_raw(1)));
        assert_eq!(output.bitrate(), 8_000_000);
        assert!(!output.force_idr(false));
    }
}
