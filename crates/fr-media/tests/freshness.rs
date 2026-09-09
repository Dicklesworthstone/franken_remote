//! Real record/reassembly policy with explicit decoder/visibility completions.
//! These metadata tests are not native decode or physical-display evidence.
use fr_core::ids::{CodecConfigurationGeneration, HostBootId, RecoveryGeneration};
use fr_core::limits::ProtocolLimits;
use fr_media::{delivery::*, freshness::*};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, RecoveryChunk,
    SourceObservation, encode_progress, encode_recovery,
};

fn clock(sent: u64, host: u64, received: u64, drift: u32) -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: sent,
            host_sample_us: host,
            client_received_us: received,
        },
        ClockPolicy {
            max_exchange_us: 1_000_000,
            valid_for_us: 10_000_000,
            drift_ppm: drift,
        },
    )
    .unwrap()
}
fn setup() -> (ReceivePipeline, ViewTracker, MediaLimits) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
    let epoch = MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    let mut receive = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings,
            epoch,
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    receive.decoder_configured(10_000).unwrap();
    let view = ViewTracker::new(&receive, clock(0, 1_000_000, 10_000, 0), 250_000, 10_000).unwrap();
    (receive, view, limits)
}
fn picture(
    receive: &mut ReceivePipeline,
    limits: MediaLimits,
    frame: u64,
    captured: u64,
    arrived: u64,
) -> ReceivedPicture {
    let mut wire = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame,
            total_bytes: 4,
            offset: 0,
            capture_micros: captured,
            bytes: b"data",
        },
        2,
        &limits,
        &mut wire,
    )
    .unwrap();
    receive
        .receive(Channel::Recovery, &wire[..n], arrived)
        .unwrap();
    receive.take_decodable(arrived).unwrap().unwrap()
}
fn progress(
    view: &mut ViewTracker,
    limits: MediaLimits,
    descriptor: FrameDescriptor,
    observed: u64,
    source: SourceObservation,
    now: u64,
) -> Result<(), Error> {
    let mut wire = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: observed,
            observation: source,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut wire,
    )
    .unwrap();
    view.progress(&wire[..n], &limits, now)
}
fn shown() -> (ReceivePipeline, ViewTracker, MediaLimits, FrameDescriptor) {
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
    let token = receiver.complete_decode(&unit, 21_000).unwrap();
    view.decoded(token, true, 21_000).unwrap();
    let evidence = view.visible(0, 22_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 17_000);
    (receiver, view, limits, descriptor)
}
#[test]
fn clock_origins_and_asymmetric_exchange_never_use_half_rtt() {
    let correlation = clock(9_000_000, 100, 9_200_000, 0);
    assert_eq!(correlation.age_upper_us(100, 9_200_000), Ok(200_000));
    assert_eq!(correlation.age_upper_us(0, 9_200_000), Ok(200_100));
    assert_eq!(correlation.age_upper_us(0, 9_250_000), Ok(250_100));
    assert_eq!(
        correlation.age_upper_us(300_101, 9_200_000),
        Err(Error::FutureObservation)
    );
}
#[test]
fn drift_is_rounded_up_and_expiry_is_exclusive() {
    let correlation = clock(10, 500, 20, 1);
    assert_eq!(correlation.age_upper_us(500, 20), Ok(11));
    assert_eq!(
        correlation.age_upper_us(500, 19),
        Err(Error::ClockRegression)
    );
    assert_eq!(
        correlation.age_upper_us(500, correlation.valid_until_us()),
        Err(Error::ClockExpired)
    );
}
#[test]
fn clock_overflow_bad_policy_and_invalid_boot_are_refused() {
    assert_eq!(
        clock(0, u64::MAX, 10, 0).age_upper_us(u64::MAX, 10),
        Err(Error::ClockOverflow)
    );
    for sample in [
        ClockSample {
            host_boot: HostBootId::from_raw(0),
            client_sent_us: 0,
            host_sample_us: 1,
            client_received_us: 1,
        },
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: 2,
            host_sample_us: 1,
            client_received_us: 1,
        },
    ] {
        assert!(ClockCorrelation::new(sample, ClockPolicy::default()).is_err());
    }
    let sample = ClockSample {
        host_boot: HostBootId::from_raw(1),
        client_sent_us: u64::MAX - 1,
        host_sample_us: 1,
        client_received_us: u64::MAX,
    };
    assert!(ClockCorrelation::new(sample, ClockPolicy::default()).is_err());
}
#[test]
fn decode_and_submit_are_not_visible_readiness() {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    progress(
        &mut view,
        limits,
        unit.descriptor(),
        1_005_000,
        SourceObservation::Captured,
        20_000,
    )
    .unwrap();
    assert_eq!(view.evidence(20_000), Err(Error::NotSubmitted));
    view.decoded(
        receiver.complete_decode(&unit, 21_000).unwrap(),
        true,
        21_000,
    )
    .unwrap();
    assert_eq!(view.evidence(22_000), Err(Error::NotSubmitted));
    assert_eq!(view.visible(1, 22_000), Err(Error::Obsolete));
    assert!(view.visible(0, 22_000).is_ok());
}
#[test]
fn slow_decode_cannot_reuse_fresh_at_dequeue_snapshot() {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    assert!(unit.within_display_queue_budget());
    let token = receiver.complete_decode(&unit, 70_000).unwrap();
    assert_eq!(token.display_deadline_us(), 70_000);
    view.decoded(token, true, 70_000).unwrap();
    assert_eq!(view.visible(0, 70_000), Err(Error::QueueExpired));
    assert_eq!(view.evidence(70_001), Err(Error::NotSubmitted));
}
#[test]
fn a_stale_network_arrival_is_not_a_fresh_source() {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_000, 20_000);
    progress(
        &mut view,
        limits,
        unit.descriptor(),
        1_000,
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
    assert_eq!(view.visible(0, 22_000), Err(Error::SourceStale));
}
#[test]
fn verified_static_source_refreshes_observation_but_not_pixels() {
    let (_receiver, mut view, limits, descriptor) = shown();
    assert_eq!(view.evidence(300_000), Err(Error::SourceStale));
    progress(
        &mut view,
        limits,
        descriptor,
        1_290_000,
        SourceObservation::QualifiedUnchanged,
        300_000,
    )
    .unwrap();
    let evidence = view.evidence(301_000).unwrap();
    assert_eq!(evidence.pixel_age_upper_us, 296_000);
    assert_eq!(evidence.source_age_upper_us, 11_000);
    assert_eq!(evidence.source, SourceObservation::QualifiedUnchanged);
    assert_eq!(view.evidence(540_000), Err(Error::SourceStale));
}
#[test]
fn heartbeat_unknown_source_and_duplicates_do_not_refresh_pixels() {
    let (_receiver, mut view, limits, descriptor) = shown();
    let before = view.evidence(23_000).unwrap();
    assert_eq!(
        progress(
            &mut view,
            limits,
            descriptor,
            descriptor.capture_micros,
            SourceObservation::Captured,
            25_000
        ),
        Err(Error::Obsolete)
    );
    let later = view.evidence(26_000).unwrap();
    assert_eq!(before.serial, later.serial);
    assert_eq!(
        later.source_age_upper_us - before.source_age_upper_us,
        3_000
    );
    progress(
        &mut view,
        limits,
        descriptor,
        0,
        SourceObservation::Unknown,
        27_000,
    )
    .unwrap_err();
    progress(
        &mut view,
        limits,
        descriptor,
        1_027_000,
        SourceObservation::Unknown,
        27_000,
    )
    .unwrap();
    assert_eq!(view.evidence(27_000), Err(Error::SourceUnknown));
}
#[test]
fn progress_for_a_newer_unseen_picture_does_not_bless_old_pixels() {
    let (_receiver, mut view, limits, mut descriptor) = shown();
    descriptor.frame = 1;
    descriptor.reference = Some(0);
    descriptor.capture_micros = 1_290_000;
    progress(
        &mut view,
        limits,
        descriptor,
        1_290_000,
        SourceObservation::Captured,
        300_000,
    )
    .unwrap();
    assert_eq!(view.evidence(300_000), Err(Error::SourceStale));
}
#[test]
fn impossible_captured_refresh_and_descriptor_changes_are_terminal() {
    let (_receiver, mut view, limits, descriptor) = shown();
    assert_eq!(
        progress(
            &mut view,
            limits,
            descriptor,
            descriptor.capture_micros + 1,
            SourceObservation::Captured,
            25_000
        ),
        Err(Error::InvalidProgress)
    );
    assert!(view.is_closed());
    let (_receiver, mut view, limits, mut descriptor) = shown();
    descriptor.total_bytes += 1;
    assert_eq!(
        progress(
            &mut view,
            limits,
            descriptor,
            descriptor.capture_micros,
            SourceObservation::Captured,
            25_000
        ),
        Err(Error::InvalidProgress)
    );
    assert!(view.is_closed());
}
#[test]
fn foreign_generations_and_closed_or_hidden_views_cannot_be_reanimated() {
    let (mut receiver, mut view, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let token = receiver.complete_decode(&unit, 20_001).unwrap();
    let wrong = MediaBindings::new(5, 6, 7, 8).unwrap();
    let other_receive = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings: wrong,
            epoch: token.epoch(),
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    let mut other = ViewTracker::new(
        &other_receive,
        clock(0, 1_000_000, 10_000, 0),
        250_000,
        10_000,
    )
    .unwrap();
    assert_eq!(other.decoded(token, true, 21_000), Err(Error::StaleBinding));
    view.close();
    assert_eq!(view.visible(0, 22_000), Err(Error::Closed));
    let (_receiver, mut view, _, _) = shown();
    view.hide();
    assert_eq!(view.evidence(23_000), Err(Error::NotSubmitted));
    assert_eq!(view.visible(0, 23_000), Err(Error::NotSubmitted));
}
#[test]
fn clock_regression_closes_and_new_host_boot_cannot_replace_clock() {
    let (_receiver, mut view, _, _) = shown();
    let correlation = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(2),
            client_sent_us: 30_000,
            host_sample_us: 1_030_000,
            client_received_us: 30_001,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    assert_eq!(
        view.synchronize(correlation, 30_002),
        Err(Error::StaleBinding)
    );
    assert_eq!(view.evidence(21_999), Err(Error::ClockRegression));
    assert_eq!(view.evidence(31_000), Err(Error::Closed));
}
#[test]
fn successful_decode_token_does_not_release_borrowed_buffer_or_ack_twice() {
    let (mut receiver, _, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let used = receiver.budget_usage();
    let _token = receiver.complete_decode(&unit, 21_000).unwrap();
    assert_eq!(receiver.budget_usage(), used);
    assert!(receiver.complete_decode(&unit, 21_000).is_err());
    drop(unit);
    assert_eq!(receiver.budget_usage().pictures, 0);
}

#[test]
fn receiver_failure_drop_and_numeric_binding_reuse_invalidate_visibility() {
    let (receiver, mut view, _, _) = shown();
    drop(receiver);
    assert_eq!(view.evidence(23_000), Err(Error::StaleBinding));
    let (mut receiver, mut view, _, _) = shown();
    receiver.close();
    assert_eq!(view.evidence(23_000), Err(Error::StaleBinding));
    let (mut receiver, _, limits) = setup();
    let unit = picture(&mut receiver, limits, 0, 1_005_000, 20_000);
    let token = receiver.complete_decode(&unit, 21_000).unwrap();
    let (_second, mut new_view, _) = setup();
    assert_eq!(
        new_view.decoded(token, true, 21_000),
        Err(Error::StaleBinding)
    );
}
