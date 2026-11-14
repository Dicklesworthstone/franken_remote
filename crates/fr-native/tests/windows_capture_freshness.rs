//! Exact production source-evidence policy, portable without a DXGI device.
//! These tests establish the decisions at the native boundary, not hardware
//! capture, secure-desktop behavior or Windows driver qualification.
#![forbid(unsafe_code)]

#[path = "../src/windows_capture/freshness.rs"]
mod freshness;

use freshness::{Capture, Frame, Observation, Source};

fn refuses_every_capture(observation: Observation) {
    for force_idr in [false, true] {
        for only_if_changed in [false, true] {
            for pointer_moved in [false, true] {
                assert_eq!(
                    observation.capture(force_idr || !only_if_changed || pointer_moved),
                    Capture::Refuse,
                    "unknown source: force={force_idr}, changed_only={only_if_changed}, pointer={pointer_moved}"
                );
            }
        }
    }
}

#[test]
fn startup_timeout_and_pointer_updates_cannot_initialize_pixels() {
    let mut source = Source::default();
    assert!(!source.has_image());
    refuses_every_capture(source.unchanged());
    refuses_every_capture(source.released(Frame::PointerOnly, true));
    assert!(!source.has_image());

    assert_eq!(source.released(Frame::Image, true), Observation::Image);
    assert!(source.has_image());
    assert_eq!(source.unchanged(), Observation::Unchanged);
}

#[test]
fn a_healthy_static_duplication_verifies_without_encoding() {
    let mut source = Source::default();
    assert_eq!(
        source.released(Frame::Image, true).capture(false),
        Capture::Encode
    );
    for _ in 0..100 {
        assert_eq!(source.unchanged().capture(false), Capture::Unchanged);
        assert_eq!(
            source.released(Frame::PointerOnly, true).capture(false),
            Capture::Unchanged
        );
    }
    // A newly verified static image can service an explicit recovery or
    // pointer redraw. Its preceding encoded image alone could not do so.
    assert_eq!(source.unchanged().capture(true), Capture::Encode);
}

#[test]
fn lost_or_recreated_duplication_cannot_verify_or_retimestamp_cached_pixels() {
    let mut source = Source::default();
    source.released(Frame::Image, true);
    source.invalidate();
    assert!(!source.has_image());
    // Both denied recreation and a newly created interface without its
    // first image leave exactly this state. Neither a timeout nor a cursor
    // callback constitutes the missing desktop image.
    for _ in 0..3 {
        refuses_every_capture(source.unchanged());
        refuses_every_capture(source.released(Frame::PointerOnly, true));
    }

    assert_eq!(
        source.released(Frame::Image, true).capture(false),
        Capture::Encode
    );
    assert_eq!(source.unchanged().capture(false), Capture::Unchanged);
}

#[test]
fn release_failure_discards_copied_pixels_and_prior_evidence() {
    for previously_current in [false, true] {
        for frame in [Frame::Image, Frame::PointerOnly] {
            let mut source = Source::default();
            if previously_current {
                source.released(Frame::Image, true);
            }
            // ReleaseFrame can report ACCESS_LOST after CopyResource was
            // queued successfully. Even those newly copied bytes cannot be
            // published under the new request's timestamp.
            refuses_every_capture(source.released(frame, false));
            assert!(!source.has_image());
            refuses_every_capture(source.unchanged());
            refuses_every_capture(source.released(Frame::PointerOnly, true));
        }
    }
}

#[test]
fn failed_image_copy_does_not_leave_old_pixels_current() {
    let mut source = Source::default();
    source.released(Frame::Image, true);
    refuses_every_capture(source.released(Frame::Failed, true));
    assert!(!source.has_image());
    refuses_every_capture(source.unchanged());
    assert_eq!(
        source.released(Frame::Image, true).capture(true),
        Capture::Encode
    );
}

#[test]
fn repeated_loss_requires_a_new_successful_image_each_time() {
    let mut source = Source::default();
    for _ in 0..10 {
        source.invalidate();
        refuses_every_capture(source.released(Frame::PointerOnly, true));
        refuses_every_capture(source.released(Frame::Image, false));
        assert_eq!(
            source.released(Frame::Image, true).capture(false),
            Capture::Encode
        );
        assert!(source.has_image());
        assert_eq!(source.unchanged().capture(false), Capture::Unchanged);
    }
}
