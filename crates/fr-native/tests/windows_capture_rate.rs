//! Exact Windows capture/rate arbitration and the shared dual-clock policy.
//! No native device or fixture encoder is used; hardware qualification is separate.
#![forbid(unsafe_code)]

#[path = "../src/capture/rate.rs"]
mod capture_rate;
#[path = "../src/windows_capture/freshness.rs"]
mod freshness;
#[path = "../src/windows_capture/plan.rs"]
mod plan;

use capture_rate::Rate;
use freshness::{Frame, Observation, Source};
use plan::{Picture, Plan, Work};
use std::time::{Duration, Instant};

fn work(observation: Observation) -> Work {
    Work {
        observation,
        pointer_changed: false,
        only_if_changed: true,
        picture: Picture::Following,
    }
}

#[test]
fn a_pending_rate_cannot_wake_a_healthy_static_desktop() {
    let start = Instant::now();
    let mut source = Source::default();
    assert!(!source.has_image());
    source.released(Frame::Image, true);
    assert!(source.has_image());
    let mut rate = Rate::new(8_000_000);
    rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
    for host in [0, 10, 1_000_000, 60_000_000] {
        assert_eq!(
            work(source.unchanged()).plan(&rate, host, start + Duration::from_micros(host)),
            Plan::Unchanged
        );
        assert_eq!(rate.applied_bitrate(), 8_000_000);
    }
    // The first real change can use the original unspent target.
    assert_eq!(
        work(source.released(Frame::Image, true)).plan(&rate, 60_000_001, start),
        Plan::Encode {
            bitrate: 4_000_000,
            force_idr: true,
        }
    );
}

#[test]
fn changed_pixels_cursor_redraw_and_required_recovery_share_one_rate_transaction() {
    let start = Instant::now();
    let mut rate = Rate::new(8_000_000);
    rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
    for work in [
        work(Observation::Image),
        Work {
            pointer_changed: true,
            ..work(Observation::Unchanged)
        },
        Work {
            picture: Picture::Recovery,
            ..work(Observation::Unchanged)
        },
        Work {
            only_if_changed: false,
            ..work(Observation::Unchanged)
        },
    ] {
        assert_eq!(
            work.plan(&rate, 1, start),
            Plan::Encode {
                bitrate: 4_000_000,
                force_idr: true,
            }
        );
    }
}

#[test]
fn bootstrap_keeps_the_opening_rate_even_with_a_pending_reduction() {
    let start = Instant::now();
    let mut rate = Rate::new(8_000_000);
    rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
    for observation in [Observation::Image, Observation::Unchanged] {
        let bootstrap = Work {
            picture: Picture::First,
            ..work(observation)
        };
        assert_eq!(
            bootstrap.plan(&rate, 1, start),
            Plan::Encode {
                bitrate: 8_000_000,
                force_idr: true,
            }
        );
    }
}

#[test]
fn a_rate_target_or_recovery_cannot_bypass_unknown_or_lost_source_evidence() {
    let start = Instant::now();
    let mut source = Source::default();
    let mut rate = Rate::new(8_000_000);
    rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
    for observation in [
        source.unchanged(),
        source.released(Frame::PointerOnly, true),
        source.released(Frame::Image, false),
        source.released(Frame::Failed, true),
    ] {
        assert_eq!(observation, Observation::Unknown);
        for picture in [Picture::First, Picture::Following, Picture::Recovery] {
            for pointer_changed in [false, true] {
                for only_if_changed in [false, true] {
                    let work = Work {
                        pointer_changed,
                        only_if_changed,
                        picture,
                        ..work(observation)
                    };
                    assert_eq!(work.plan(&rate, 1, start), Plan::Refuse);
                }
            }
        }
    }
    source.released(Frame::Image, true);
    source.invalidate();
    assert_eq!(work(source.unchanged()).plan(&rate, 1, start), Plan::Refuse);
    assert_eq!(rate.applied_bitrate(), 8_000_000);
}

#[test]
fn rate_dwell_uses_both_clocks_and_does_not_force_idrs_while_deferred() {
    let start = Instant::now();
    let mut rate = Rate::new(8_000_000);
    rate.request_with_ceiling(4_000_000, 8_000_000).unwrap();
    rate.confirm(4_000_000, 4_000_000, true, 10, start).unwrap();
    rate.request_with_ceiling(2_000_000, 8_000_000).unwrap();
    let changed = work(Observation::Image);
    for (host, worker) in [
        (2_000_009, start + Duration::from_secs(2)),
        (2_000_010, start + Duration::from_micros(1_999_999)),
    ] {
        assert_eq!(
            changed.plan(&rate, host, worker),
            Plan::Encode {
                bitrate: 4_000_000,
                force_idr: false,
            }
        );
    }
    assert_eq!(
        changed.plan(&rate, 2_000_010, start + Duration::from_secs(2)),
        Plan::Encode {
            bitrate: 2_000_000,
            force_idr: true,
        }
    );
    // Cancelling with the actual rate consumes no native property write.
    rate.request(rate.applied_bitrate());
    assert_eq!(
        changed.plan(&rate, 2_000_010, start + Duration::from_secs(2)),
        Plan::Encode {
            bitrate: 4_000_000,
            force_idr: false,
        }
    );
}
