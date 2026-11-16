//! Reference-only decoding must not consume a renderer's pending visibility
//! callback. These exercise the real receiver-issued decode tokens and original
//! display deadlines. These are metadata tests, not native display evidence.
use super::*;

fn pending() -> (ReceivePipeline, ViewTracker, MediaLimits, FrameDescriptor) {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let descriptor = unit.descriptor();
    progress(
        &mut view,
        limits,
        descriptor,
        descriptor.capture_micros,
        SourceObservation::Captured,
        20_000,
    )
    .unwrap();
    view.decoded(
        receiver.complete_decode(&unit, 21_000).unwrap(),
        true,
        21_000,
    )
    .unwrap();
    (receiver, view, limits, descriptor)
}

fn decode_next(
    receiver: &mut ReceivePipeline,
    view: &mut ViewTracker,
    limits: MediaLimits,
    frame: u64,
    submitted: bool,
    now: u64,
) {
    let descriptor = FrameDescriptor {
        frame,
        reference: Some(frame - 1),
        total_bytes: 4,
        stride: limits.fragment_stride(),
        capture_micros: 1_000_000 + now - 1_000,
    };
    let mut wire = [0; 1150];
    let size = fr_wire::encode_fragment(
        fr_wire::Fragment {
            descriptor,
            index: 0,
            bytes: b"data",
        },
        1,
        &limits,
        &mut wire,
    )
    .unwrap();
    receiver
        .receive(Channel::Video, &wire[..size], now)
        .unwrap();
    let unit = receiver.take_decodable(now).unwrap().unwrap();
    progress(
        view,
        limits,
        unit.descriptor(),
        unit.descriptor().capture_micros,
        SourceObservation::Captured,
        now,
    )
    .unwrap();
    view.decoded(
        receiver.complete_decode(&unit, now + 1).unwrap(),
        submitted,
        now + 1,
    )
    .unwrap();
}

#[test]
fn decoding_without_submission_preserves_the_pending_visible_picture() {
    let (mut receiver, mut view, limits, original) = pending();
    decode_next(&mut receiver, &mut view, limits, 1, false, 22_000);
    assert_eq!(view.evidence(23_000), Err(Error::NotSubmitted));
    // The newer reference picture was not submitted and cannot stand for the
    // older picture still waiting on its original compositor callback.
    assert_eq!(view.visible(1, 23_000), Err(Error::Obsolete));
    let evidence = view.visible(original.frame, 24_000).unwrap();
    assert_eq!(evidence.frame, original.frame);
    assert_eq!(evidence.pixel_age_upper_us, 19_000);
    assert_eq!(evidence.source_age_upper_us, 19_000);
    assert_eq!(
        view.presented_sample(24_001).unwrap().stamp.frame,
        original.frame
    );
}

#[test]
fn decoding_without_submission_never_renews_the_pending_display_deadline() {
    let (mut receiver, mut view, limits, original) = pending();
    decode_next(&mut receiver, &mut view, limits, 1, false, 30_000);
    assert_eq!(
        view.visible(original.frame, 70_000),
        Err(Error::QueueExpired)
    );
    assert_eq!(view.evidence(70_001), Err(Error::NotSubmitted));
    assert_eq!(view.evidence(320_000), Err(Error::SourceStale));
}

#[test]
fn a_new_submission_still_replaces_the_original_pending_picture() {
    let (mut receiver, mut view, limits, original) = pending();
    decode_next(&mut receiver, &mut view, limits, 1, false, 22_000);
    decode_next(&mut receiver, &mut view, limits, 2, true, 24_000);
    assert_eq!(view.visible(original.frame, 25_000), Err(Error::Obsolete));
    assert_eq!(view.visible(1, 25_000), Err(Error::Obsolete));
    assert_eq!(view.visible(2, 26_000).unwrap().frame, 2);
}

#[test]
fn hiding_or_retiring_the_receiver_still_fences_a_preserved_candidate() {
    let (mut receiver, mut view, limits, original) = pending();
    decode_next(&mut receiver, &mut view, limits, 1, false, 22_000);
    view.hide();
    decode_next(&mut receiver, &mut view, limits, 2, false, 24_000);
    assert_eq!(
        view.visible(original.frame, 25_000),
        Err(Error::NotSubmitted)
    );
    assert_eq!(view.visible(2, 25_000), Err(Error::NotSubmitted));

    let (mut receiver, mut view, limits, original) = pending();
    decode_next(&mut receiver, &mut view, limits, 1, false, 22_000);
    receiver.close();
    assert_eq!(
        view.visible(original.frame, 24_000),
        Err(Error::StaleBinding)
    );
    assert!(view.is_closed());
}
