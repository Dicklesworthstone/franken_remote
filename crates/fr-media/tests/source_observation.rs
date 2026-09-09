//! Actual packetizer/record/receiver policy; synthetic pixels and clock only.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::*,
    freshness::{ClockCorrelation, ClockPolicy, ClockSample, ViewTracker},
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, SourceObservation,
    decode_progress,
};

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn progress(frame: u64, reference: Option<u64>, now: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            reference,
            total_bytes: 2000,
            stride: config().limits.fragment_stride(),
            capture_micros: now,
        },
        observed_micros: now,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn sender() -> SendCache {
    let c = config();
    SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap()
}
fn packets(cache: &mut SendCache, now: u64) -> Vec<(PacketOffer, Vec<u8>)> {
    let mut out = [0; 1024];
    let mut result = Vec::new();
    while let Some(offer) = cache.next_packet(now, &mut out).unwrap() {
        cache.authorize_write(&offer, now).unwrap();
        let bytes = out[..offer.byte_len()].to_vec();
        result.push((offer, bytes));
    }
    result
}
fn decode(bytes: &[u8]) -> Progress {
    let c = config();
    decode_progress(
        Record::decode(
            bytes,
            &c.limits,
            c.bindings.for_channel(Channel::MediaConfig),
            Channel::MediaConfig,
        )
        .unwrap(),
        &c.limits,
    )
    .unwrap()
}
fn bootstrap() -> (SendCache, ReceivePipeline) {
    let c = config();
    let mut tx = sender();
    let mut rx = ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    rx.decoder_configured(0).unwrap();
    tx.push(
        progress(0, None, 0),
        vec![7; 2000],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    for (o, b) in packets(&mut tx, 0) {
        rx.receive(o.channel(), &b, 0).unwrap();
    }
    (tx, rx)
}
#[test]
fn actual_idle_progress_survives_payload_eviction_without_extending_pixel_age() {
    let (mut tx, mut rx) = bootstrap();
    let c = config();
    let picture = rx.take_decodable(0).unwrap().unwrap();
    let decoded = rx.complete_decode(&picture, 0).unwrap();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: fr_core::ids::HostBootId::from_raw(1),
            client_sent_us: 0,
            host_sample_us: 0,
            client_received_us: 0,
        },
        ClockPolicy {
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let mut view = ViewTracker::new(&rx, clock, 250_000, 0).unwrap();
    view.progress(&packets_for_progress(progress(0, None, 0)), &c.limits, 0)
        .unwrap();
    view.decoded(decoded, true, 0).unwrap();
    view.visible(0, 0).unwrap();
    drop(picture);
    for n in 1..=24 {
        let now = n * 100_000;
        assert!(tx.observe_unchanged(0, now, now).unwrap());
        let updates = packets(&mut tx, now);
        assert_eq!(updates.len(), 1);
        let (offer, bytes) = &updates[0];
        assert_eq!(offer.channel(), Channel::MediaConfig);
        let p = decode(bytes);
        assert_eq!(p.descriptor, progress(0, None, 0).descriptor);
        assert_eq!(p.observation, SourceObservation::QualifiedUnchanged);
        assert_eq!(p.pipeline, PipelineState::Idle);
        rx.receive(offer.channel(), bytes, now).unwrap();
        view.progress(bytes, &c.limits, now).unwrap();
        let evidence = view.evidence(now).unwrap();
        assert!(evidence.pixel_age_upper_us >= now);
        assert!(evidence.source_age_upper_us < 250_000);
        assert!(rx.take_decodable(now).unwrap().is_none());
    }
    assert_eq!(tx.cached_pictures(), 0);
    assert_eq!(tx.cached_bytes(), 0);
    assert_eq!(rx.budget_usage(), BudgetUsage::default());
    assert_eq!(c.epoch, view.epoch());
    assert!(!tx.needs_recovery());
}
fn packets_for_progress(p: Progress) -> Vec<u8> {
    let mut out = vec![0; 1024];
    let c = config();
    let n = fr_wire::encode_progress(
        p,
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut out,
    )
    .unwrap();
    out.truncate(n);
    out
}
#[test]
fn coalesces_fixed_metadata_and_never_slides_a_prepared_deadline() {
    let (mut tx, _) = bootstrap();
    tx.tick(2_000_000).unwrap();
    for now in 2_000_001..=2_001_000 {
        assert!(tx.observe_unchanged(0, now, now).unwrap());
    }
    assert_eq!(tx.cached_bytes(), 0);
    assert_eq!(tx.cached_pictures(), 0);
    assert_eq!(tx.next_deadline(), Some(2_251_000));
    assert!(tx.next_packet(2_001_000, &mut [0; 10]).is_err());
    assert_eq!(tx.next_deadline(), Some(2_251_000));
    let updates = packets(&mut tx, 2_010_000);
    assert_eq!(updates.len(), 1);
    assert_eq!(decode(&updates[0].1).observed_micros, 2_001_000);
    let offer = &updates[0].0;
    assert_eq!(offer.send_by_micros(), 2_251_000);
    tx.authorize_write(offer, 2_250_999).unwrap();
    assert_eq!(
        tx.authorize_write(offer, 2_251_000),
        Err(SendError::OriginalExpired)
    );
    assert!(!tx.observe_unchanged(0, 2_001_000, 2_251_000).unwrap());
    assert_eq!(packets(&mut tx, 2_251_000).len(), 0);
}
#[test]
fn stale_metadata_drops_without_poisoning_codec_reference_state() {
    let (mut tx, _) = bootstrap();
    tx.tick(2_000_000).unwrap();
    assert!(tx.observe_unchanged(0, 2_000_001, 2_000_002).unwrap());
    tx.tick(2_250_001).unwrap();
    assert_eq!(packets(&mut tx, 2_250_001).len(), 0);
    assert!(!tx.needs_recovery());
    assert_eq!(tx.next_deadline(), None);
    assert_eq!(
        tx.observe_unchanged(0, 2_250_002, 2_500_002),
        Err(SendError::ObservationExpired)
    );
    assert!(tx.observe_unchanged(0, 2_500_003, 2_500_003).unwrap());
    assert_eq!(packets(&mut tx, 2_500_003).len(), 1);
    tx.push(
        progress(30, Some(0), 2_500_004),
        vec![8; 2000],
        DeliveryMode::Datagrams,
        2_500_004,
    )
    .unwrap();
    assert!(
        packets(&mut tx, 2_500_004)
            .iter()
            .any(|(o, _)| o.channel() == Channel::Video)
    );
}
#[test]
fn original_startup_precedes_observations_and_new_frames_discard_obsolete_metadata() {
    let mut tx = sender();
    tx.push(
        progress(0, None, 0),
        vec![7; 2000],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    tx.observe_unchanged(0, 1, 1).unwrap();
    let p = packets(&mut tx, 1);
    assert_eq!(decode(&p[0].1).observation, SourceObservation::Captured);
    assert!(
        p[1..p.len() - 1]
            .iter()
            .all(|(o, _)| o.channel() == Channel::Recovery)
    );
    assert_eq!(
        decode(&p.last().unwrap().1).observation,
        SourceObservation::QualifiedUnchanged
    );
    tx.observe_unchanged(0, 2, 2).unwrap();
    tx.push(
        progress(25, Some(0), 3),
        vec![8; 2000],
        DeliveryMode::Datagrams,
        3,
    )
    .unwrap();
    let p = packets(&mut tx, 3);
    assert!(p.iter().all(|(o, _)| o.frame() == 25));
    assert_eq!(decode(&p[0].1).observation, SourceObservation::Captured);
}
#[test]
fn observations_cannot_invent_anchors_rewrite_timestamps_or_repair_lost_originals() {
    let mut tx = sender();
    assert_eq!(
        tx.observe_unchanged(0, 0, 0),
        Err(SendError::InvalidObservation)
    );
    tx.push(
        progress(0, None, 5),
        vec![7; 2000],
        DeliveryMode::Recovery,
        5,
    )
    .unwrap();
    for (frame, observed, now) in [(1, 6, 6), (0, 4, 6), (0, 7, 6)] {
        assert_eq!(
            tx.observe_unchanged(frame, observed, now),
            Err(SendError::InvalidObservation)
        );
    }
    assert!(!tx.observe_unchanged(0, 5, 6).unwrap());
    assert!(tx.observe_unchanged(0, 8, 8).unwrap());
    assert_eq!(
        tx.observe_unchanged(0, 2_000_005, 2_000_005),
        Err(SendError::OriginalExpired)
    );
    assert!(tx.needs_recovery());
    assert_eq!(tx.cached_bytes(), 0);
    assert_eq!(
        tx.observe_unchanged(0, 2_000_006, 2_000_006),
        Err(SendError::NeedsRecovery)
    );
    let mut tx = sender();
    let mut p = progress(0, None, 0);
    p.observation = SourceObservation::Unknown;
    tx.push(p, vec![7; 2000], DeliveryMode::Recovery, 0)
        .unwrap();
    assert_eq!(
        tx.observe_unchanged(0, 1, 1),
        Err(SendError::InvalidObservation)
    );
}
#[test]
fn replacement_and_foreign_sender_fence_observation_offers_without_reusable_identity() {
    let (mut tx, _) = bootstrap();
    tx.observe_unchanged(0, 1, 1).unwrap();
    let offer = packets(&mut tx, 1).pop().unwrap().0;
    assert!(sender().authorize_write(&offer, 2).is_err());
    let c = config();
    tx.replace(
        MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        MediaBindings::new(5, 6, 7, 8).unwrap(),
        2,
    )
    .unwrap();
    assert!(tx.authorize_write(&offer, 2).is_err());
    assert_eq!(
        tx.observe_unchanged(0, 3, 3),
        Err(SendError::InvalidObservation)
    );
    tx.close();
    assert_eq!(tx.observe_unchanged(0, 4, 4), Err(SendError::Closed));
}
