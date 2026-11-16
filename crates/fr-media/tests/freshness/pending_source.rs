//! A progress record for a newer unseen picture cannot manufacture source
//! evidence for a pending picture. These exercise real records and decode tokens,
//! not a native compositor or physical-display qualification.
use super::*;

fn ready() -> (ReceivePipeline, ViewTracker, MediaLimits, ReceivedPicture) {
    let (mut receiver, view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    (receiver, view, limits, unit)
}

fn next_progress(
    view: &mut ViewTracker,
    limits: MediaLimits,
    original: FrameDescriptor,
    frame: u64,
    source: SourceObservation,
    now: u64,
) {
    let descriptor = FrameDescriptor {
        frame,
        reference: Some(frame - 1),
        capture_micros: 1_000_000 + now - 1_000,
        ..original
    };
    progress(
        view,
        limits,
        descriptor,
        descriptor.capture_micros,
        source,
        now,
    )
    .unwrap();
}

fn submit(
    receiver: &mut ReceivePipeline,
    view: &mut ViewTracker,
    unit: &ReceivedPicture,
    now: u64,
) {
    view.decoded(receiver.complete_decode(unit, now).unwrap(), true, now)
        .unwrap();
}

#[test]
fn newer_progress_does_not_erase_a_pending_pictures_unknown_source() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    submit(&mut receiver, &mut view, &unit, 21_000);
    progress(
        &mut view,
        limits,
        original,
        0,
        SourceObservation::Unknown,
        22_000,
    )
    .unwrap();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        23_000,
    );
    assert_eq!(
        view.visible(original.frame, 24_000),
        Err(Error::SourceUnknown)
    );
    // SourceUnknown (not NotSubmitted) retains the picture actually on screen,
    // but cannot issue a positive presented report or enable input with it.
    assert_eq!(view.evidence(25_000), Err(Error::SourceUnknown));
    assert_eq!(view.presented_sample(25_001), Err(Error::SourceUnknown));
    assert!(!view.is_closed());
}

#[test]
fn unknown_source_before_decode_survives_a_newer_progress_record() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        0,
        SourceObservation::Unknown,
        20_000,
    )
    .unwrap();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        21_000,
    );
    submit(&mut receiver, &mut view, &unit, 22_000);
    assert_eq!(
        view.visible(original.frame, 23_000),
        Err(Error::SourceUnknown)
    );
    assert_eq!(view.evidence(24_000), Err(Error::SourceUnknown));
}

#[test]
fn multiple_unknown_reports_cannot_forget_an_earlier_undecoded_picture() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        u64::MAX,
        SourceObservation::Unknown,
        20_000,
    )
    .unwrap();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Unknown,
        21_000,
    );
    next_progress(
        &mut view,
        limits,
        original,
        2,
        SourceObservation::Captured,
        22_000,
    );
    submit(&mut receiver, &mut view, &unit, 23_000);
    assert_eq!(
        view.visible(original.frame, 24_000),
        Err(Error::SourceUnknown)
    );
}

#[test]
fn pending_unchanged_source_keeps_its_exact_stamp_and_original_age() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        original.capture_micros,
        SourceObservation::Captured,
        20_000,
    )
    .unwrap();
    submit(&mut receiver, &mut view, &unit, 21_000);
    progress(
        &mut view,
        limits,
        original,
        1_022_000,
        SourceObservation::QualifiedUnchanged,
        23_000,
    )
    .unwrap();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        24_000,
    );
    let evidence = view.visible(original.frame, 25_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 20_000);
    assert_eq!(evidence.source_age_upper_us, 3_000);
    assert_eq!(evidence.source, SourceObservation::QualifiedUnchanged);
    let stamp = view.presented_sample(25_001).unwrap().stamp;
    assert_eq!(stamp.frame, original.frame);
    assert_eq!(stamp.captured_us, original.capture_micros);
    assert_eq!(stamp.observed_us, 1_022_000);
    assert_eq!(view.evidence(272_000), Err(Error::SourceStale));
}

#[test]
fn exact_positive_progress_can_requalify_the_pending_picture() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        0,
        SourceObservation::Unknown,
        20_000,
    )
    .unwrap();
    submit(&mut receiver, &mut view, &unit, 21_000);
    progress(
        &mut view,
        limits,
        original,
        1_021_000,
        SourceObservation::QualifiedUnchanged,
        22_000,
    )
    .unwrap();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        23_000,
    );
    let evidence = view.visible(original.frame, 24_000).unwrap();
    assert_eq!(evidence.source, SourceObservation::QualifiedUnchanged);
    assert_eq!(evidence.source_age_upper_us, 3_000);
}

#[test]
fn exact_positive_progress_at_decode_overrides_the_unknown_floor() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        0,
        SourceObservation::Unknown,
        20_000,
    )
    .unwrap();
    progress(
        &mut view,
        limits,
        original,
        1_021_000,
        SourceObservation::QualifiedUnchanged,
        22_000,
    )
    .unwrap();
    submit(&mut receiver, &mut view, &unit, 23_000);
    let evidence = view.visible(original.frame, 24_000).unwrap();
    assert_eq!(evidence.source, SourceObservation::QualifiedUnchanged);
    assert_eq!(evidence.source_age_upper_us, 3_000);
}

#[test]
fn newer_progress_without_unknown_history_keeps_capture_evidence_usable() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        21_000,
    );
    submit(&mut receiver, &mut view, &unit, 22_000);
    let evidence = view.visible(original.frame, 23_000).unwrap();
    assert_eq!(evidence.source, SourceObservation::Captured);
    assert_eq!(evidence.pixel_age_upper_us, 18_000);
    assert_eq!(evidence.source_age_upper_us, 18_000);
}

#[test]
fn hiding_does_not_erase_unknown_source_history_before_submission() {
    let (mut receiver, mut view, limits, unit) = ready();
    let original = unit.descriptor();
    progress(
        &mut view,
        limits,
        original,
        0,
        SourceObservation::Unknown,
        20_000,
    )
    .unwrap();
    view.hide();
    next_progress(
        &mut view,
        limits,
        original,
        1,
        SourceObservation::Captured,
        21_000,
    );
    submit(&mut receiver, &mut view, &unit, 22_000);
    assert_eq!(
        view.visible(original.frame, 23_000),
        Err(Error::SourceUnknown)
    );
}
