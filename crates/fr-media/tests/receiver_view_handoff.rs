//! Production receiver progress seeded into its exact presentation owner.
//! Decode/visibility here are explicit metadata fixtures, not native pixels.
use fr_core::{
    ids::{CodecConfigurationGeneration, HostBootId, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{delivery::*, freshness::*};
use fr_wire::*;
fn receiver() -> (ReceivePipeline, MediaLimits) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let config = ReceiveConfig {
        limits,
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    };
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(10_000).unwrap();
    (receiver, limits)
}
fn progress(
    receiver: &mut ReceivePipeline,
    limits: &MediaLimits,
    descriptor: FrameDescriptor,
    stamp: u64,
    source: SourceObservation,
    at: u64,
) {
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: source,
            pipeline: PipelineState::Running,
        },
        3,
        limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::MediaConfig, &bytes[..n], at)
        .unwrap();
}
fn shown() -> (ReceivePipeline, ViewTracker, MediaLimits, FrameDescriptor) {
    let (mut receiver, limits) = receiver();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: 0,
            host_sample_us: 1_000_000,
            client_received_us: 10_000,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: 1_005_000,
            bytes: b"data",
        },
        2,
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], 20_000)
        .unwrap();
    let unit = receiver.take_decodable(20_000).unwrap().unwrap();
    let descriptor = unit.descriptor();
    let decoded = receiver.complete_decode(&unit, 21_000).unwrap();
    progress(
        &mut receiver,
        &limits,
        descriptor,
        1_005_000,
        SourceObservation::Captured,
        21_000,
    );
    let mut view = ViewTracker::new(&receiver, clock, 250_000, 22_000).unwrap();
    view.observe_receiver(&receiver, 22_000).unwrap();
    view.decoded(decoded, true, 22_000).unwrap();
    view.visible(0, 23_000).unwrap();
    (receiver, view, limits, descriptor)
}
#[test]
fn admitted_receiver_progress_seeds_view_without_retiming_source_or_redecoding() {
    let (receiver, mut view, _, _) = shown();
    let initial = view.evidence(23_000).unwrap();
    let usage = receiver.budget_usage();
    for at in [30_000, 100_000, 254_999] {
        view.observe_receiver(&receiver, at).unwrap();
        let evidence = view.evidence(at).unwrap();
        assert_eq!(evidence.serial, initial.serial);
        assert_eq!(evidence.source_age_upper_us, at - 5_000);
        assert_eq!(evidence.pixel_age_upper_us, at - 5_000);
    }
    view.observe_receiver(&receiver, 255_000).unwrap();
    assert_eq!(view.evidence(255_000), Err(Error::SourceStale));
    assert_eq!(receiver.budget_usage(), usage);
}
#[test]
fn only_new_original_receiver_observations_can_refresh_the_existing_pixels() {
    let (mut receiver, mut view, limits, descriptor) = shown();
    progress(
        &mut receiver,
        &limits,
        descriptor,
        1_100_000,
        SourceObservation::QualifiedUnchanged,
        120_000,
    );
    view.observe_receiver(&receiver, 120_000).unwrap();
    let evidence = view.evidence(120_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 115_000);
    assert_eq!(evidence.source_age_upper_us, 20_000);
    view.observe_receiver(&receiver, 349_999).unwrap();
    assert_eq!(view.evidence(349_999).unwrap().serial, evidence.serial);
    assert_eq!(view.evidence(350_000), Err(Error::SourceStale));
}
#[test]
fn foreign_equal_numeric_receiver_never_changes_the_original_view() {
    let (original, mut view, _, _) = shown();
    let (foreign, _) = receiver();
    assert_eq!(
        view.observe_receiver(&foreign, 30_000),
        Err(Error::StaleBinding)
    );
    view.observe_receiver(&original, 30_000).unwrap();
    assert_eq!(view.evidence(30_000).unwrap().source_age_upper_us, 25_000);
}
#[test]
fn closed_original_and_regressed_clock_cannot_supply_handoff_evidence() {
    let (mut receiver, mut view, _, _) = shown();
    receiver.close();
    assert_eq!(
        view.observe_receiver(&receiver, 30_000),
        Err(Error::StaleBinding)
    );
    let (receiver, mut view, _, _) = shown();
    assert_eq!(
        view.observe_receiver(&receiver, 22_999),
        Err(Error::ClockRegression)
    );
    assert!(view.is_closed());
    assert!(view.observe_receiver(&receiver, 30_000).is_err());
}
#[test]
fn seeded_new_unseen_progress_does_not_refresh_the_previous_visible_source() {
    let (mut receiver, mut view, limits, old) = shown();
    let newer = FrameDescriptor {
        frame: 1,
        reference: Some(old.frame),
        capture_micros: 1_100_000,
        total_bytes: 4,
        stride: 4,
    };
    progress(
        &mut receiver,
        &limits,
        newer,
        1_100_000,
        SourceObservation::Captured,
        120_000,
    );
    view.observe_receiver(&receiver, 120_000).unwrap();
    assert_eq!(view.evidence(120_000).unwrap().frame, old.frame);
    assert_eq!(view.evidence(120_000).unwrap().source_age_upper_us, 115_000);
    assert_eq!(view.evidence(255_000), Err(Error::SourceStale));
}
#[test]
fn unknown_receiver_source_cannot_be_blessed_by_repeated_handoff_polling() {
    let (mut receiver, mut view, limits, descriptor) = shown();
    progress(
        &mut receiver,
        &limits,
        descriptor,
        0,
        SourceObservation::Unknown,
        30_000,
    );
    view.observe_receiver(&receiver, 30_000).unwrap();
    assert_eq!(view.evidence(30_000), Err(Error::SourceUnknown));
    view.observe_receiver(&receiver, 40_000).unwrap();
    assert_eq!(view.evidence(40_000), Err(Error::SourceUnknown));
}
