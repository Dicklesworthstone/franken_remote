//! Delayed metadata must not turn a late picture into fresh input evidence.
use super::*;

fn progress_with_pipeline(
    view: &mut ViewTracker,
    limits: MediaLimits,
    descriptor: FrameDescriptor,
    observed: u64,
    source: SourceObservation,
    pipeline: PipelineState,
    now: u64,
) -> Result<(), Error> {
    let mut wire = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: observed,
            observation: source,
            pipeline,
        },
        3,
        &limits,
        &mut wire,
    )
    .unwrap();
    view.progress(&wire[..n], &limits, now)
}

#[test]
fn delayed_original_capture_cannot_qualify_a_picture_shown_before_progress() {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let descriptor = unit.descriptor();
    let token = receiver.complete_decode(&unit, 70_000).unwrap();
    view.decoded(token, true, 70_000).unwrap();
    assert_eq!(view.visible(0, 70_000), Err(Error::QueueExpired));

    progress(
        &mut view,
        limits,
        descriptor,
        descriptor.capture_micros,
        SourceObservation::Captured,
        71_000,
    )
    .unwrap();
    assert_eq!(view.evidence(71_000), Err(Error::NotSubmitted));
    assert_eq!(view.presented_sample(71_000), Err(Error::NotSubmitted));

    progress(
        &mut view,
        limits,
        descriptor,
        1_070_000,
        SourceObservation::QualifiedUnchanged,
        75_000,
    )
    .unwrap();
    let evidence = view.evidence(75_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 70_000);
    assert_eq!(evidence.source_age_upper_us, 5_000);
    assert_eq!(
        view.presented_sample(75_000).unwrap().stamp.observed_us,
        1_070_000
    );
}

#[test]
fn equal_time_source_and_pipeline_changes_cannot_qualify_late_pixels() {
    for (source, pipeline) in [
        (SourceObservation::Captured, PipelineState::Idle),
        (
            SourceObservation::QualifiedUnchanged,
            PipelineState::Running,
        ),
        (SourceObservation::QualifiedUnchanged, PipelineState::Idle),
    ] {
        let (_receiver, mut view, limits, descriptor) = late_picture();
        progress_with_pipeline(
            &mut view,
            limits,
            descriptor,
            descriptor.capture_micros,
            source,
            pipeline,
            71_000,
        )
        .unwrap();
        assert_eq!(view.evidence(71_000), Err(Error::NotSubmitted));
        assert_eq!(view.evidence(320_000), Err(Error::SourceStale));
    }
}

fn known_then_unknown_before_late_visibility(
    hide: bool,
) -> (ReceivePipeline, ViewTracker, MediaLimits, FrameDescriptor) {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let descriptor = unit.descriptor();
    progress(
        &mut view,
        limits,
        descriptor,
        1_050_000,
        SourceObservation::QualifiedUnchanged,
        60_000,
    )
    .unwrap();
    progress(
        &mut view,
        limits,
        descriptor,
        u64::MAX,
        SourceObservation::Unknown,
        61_000,
    )
    .unwrap();
    if hide {
        view.hide();
    }
    let token = receiver.complete_decode(&unit, 70_000).unwrap();
    view.decoded(token, true, 70_000).unwrap();
    assert_eq!(view.visible(0, 70_000), Err(Error::QueueExpired));
    (receiver, view, limits, descriptor)
}

#[test]
fn unknown_health_cannot_erase_a_known_observation_before_visibility() {
    let (_receiver, mut view, limits, descriptor) =
        known_then_unknown_before_late_visibility(false);
    assert_eq!(
        progress(
            &mut view,
            limits,
            descriptor,
            1_049_999,
            SourceObservation::QualifiedUnchanged,
            71_000,
        ),
        Err(Error::Obsolete)
    );
    progress(
        &mut view,
        limits,
        descriptor,
        1_050_000,
        SourceObservation::QualifiedUnchanged,
        72_000,
    )
    .unwrap();
    assert_eq!(view.evidence(72_000), Err(Error::NotSubmitted));

    progress(
        &mut view,
        limits,
        descriptor,
        u64::MAX,
        SourceObservation::Unknown,
        73_000,
    )
    .unwrap();
    assert_eq!(view.evidence(73_000), Err(Error::NotSubmitted));
    progress(
        &mut view,
        limits,
        descriptor,
        1_075_000,
        SourceObservation::QualifiedUnchanged,
        80_000,
    )
    .unwrap();
    let evidence = view.evidence(80_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 75_000);
    assert_eq!(evidence.source_age_upper_us, 5_000);
}

#[test]
fn hiding_and_submitting_do_not_erase_the_known_source_floor() {
    let (_receiver, mut view, limits, descriptor) = known_then_unknown_before_late_visibility(true);
    progress(
        &mut view,
        limits,
        descriptor,
        1_050_000,
        SourceObservation::QualifiedUnchanged,
        71_000,
    )
    .unwrap();
    assert_eq!(view.evidence(71_000), Err(Error::NotSubmitted));
    assert_eq!(view.evidence(320_000), Err(Error::SourceStale));
}

#[test]
fn a_new_observation_can_finish_opening_without_another_timestamp_change() {
    let (_receiver, mut view, limits, descriptor) = late_picture();
    progress_with_pipeline(
        &mut view,
        limits,
        descriptor,
        1_070_000,
        SourceObservation::QualifiedUnchanged,
        PipelineState::Opening,
        75_000,
    )
    .unwrap();
    assert_eq!(view.evidence(75_000), Err(Error::NotSubmitted));

    // The observation is newer than the frozen late-visibility floor. Its
    // transition to usable pipeline health must not require another capture.
    progress_with_pipeline(
        &mut view,
        limits,
        descriptor,
        1_070_000,
        SourceObservation::QualifiedUnchanged,
        PipelineState::Running,
        76_000,
    )
    .unwrap();
    let evidence = view.evidence(76_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 71_000);
    assert_eq!(evidence.source_age_upper_us, 6_000);
}

#[test]
fn old_and_unknown_metadata_do_not_slide_grace_but_new_verification_can_recover() {
    let (_receiver, mut view, limits, descriptor) = late_picture();
    progress(
        &mut view,
        limits,
        descriptor,
        u64::MAX,
        SourceObservation::Unknown,
        319_998,
    )
    .unwrap();
    assert_eq!(view.evidence(319_998), Err(Error::NotSubmitted));
    progress_with_pipeline(
        &mut view,
        limits,
        descriptor,
        descriptor.capture_micros,
        SourceObservation::Captured,
        PipelineState::Idle,
        319_999,
    )
    .unwrap();
    assert_eq!(view.evidence(319_999), Err(Error::NotSubmitted));
    assert_eq!(view.evidence(320_000), Err(Error::SourceStale));
    assert_eq!(view.presented_sample(320_000), Err(Error::SourceStale));

    // Source staleness can recover. The separate input authority enforces the
    // terminal suspension limit; this tracker never grants input authority.
    progress(
        &mut view,
        limits,
        descriptor,
        1_330_000,
        SourceObservation::QualifiedUnchanged,
        340_000,
    )
    .unwrap();
    let evidence = view.evidence(340_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 335_000);
    assert_eq!(evidence.source_age_upper_us, 10_000);
}

#[test]
fn a_newer_unseen_descriptor_cannot_qualify_the_late_visible_picture() {
    let (_receiver, mut view, limits, descriptor) = late_picture();
    let mut newer = descriptor;
    newer.frame += 1;
    newer.reference = Some(descriptor.frame);
    newer.capture_micros = 1_080_000;
    progress(
        &mut view,
        limits,
        newer,
        newer.capture_micros,
        SourceObservation::Captured,
        85_000,
    )
    .unwrap();
    assert_eq!(view.evidence(85_000), Err(Error::NotSubmitted));
    assert_eq!(
        progress(
            &mut view,
            limits,
            descriptor,
            1_080_000,
            SourceObservation::QualifiedUnchanged,
            86_000,
        ),
        Err(Error::Obsolete)
    );
    assert_eq!(view.evidence(320_000), Err(Error::SourceStale));
}
