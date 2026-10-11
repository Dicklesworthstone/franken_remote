//! Deterministic cache/packetization tests, not transport or codec qualification.
use super::*;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_wire::{FrameDescriptor, PipelineState, SourceObservation, encode_repair};

const HORIZON: u64 = 100_000;
fn cache() -> SendCache {
    SendCache::new(
        MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy {
            reference_horizon_micros: HORIZON,
            recovery_horizon_micros: HORIZON,
            ..SendPolicy::default()
        },
    )
    .unwrap()
}
fn picture(cache: &mut SendCache, frame: u64, now: u64) {
    cache
        .push(
            Progress {
                descriptor: FrameDescriptor {
                    frame,
                    reference: frame.checked_sub(1),
                    total_bytes: 128,
                    stride: cache.limits.fragment_stride(),
                    capture_micros: now,
                },
                observed_micros: now,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            vec![7; 128],
            if frame == 0 {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            now,
        )
        .unwrap();
}
fn drain(cache: &mut SendCache, now: u64) -> Vec<PacketOffer> {
    let mut bytes = [0; 16_384];
    let mut offers = Vec::new();
    while let Some(offer) = cache.next_packet(now, &mut bytes).unwrap() {
        cache.authorize_write(&offer, now).unwrap();
        offers.push(offer);
    }
    offers
}
fn repair(cache: &mut SendCache, now: u64) -> PacketOffer {
    let mut bytes = [0; 16_384];
    let n = encode_repair(
        1,
        &[RepairRange { start: 0, end: 1 }],
        1,
        cache.bindings.for_channel(Channel::Control),
        &cache.limits,
        &mut bytes,
    )
    .unwrap();
    cache.queue_repair(&bytes[..n], now).unwrap();
    cache.next_repair_packet(now, &mut bytes).unwrap().unwrap()
}
fn unchanged(cache: &mut SendCache, now: u64) -> PacketOffer {
    assert!(cache.observe_unchanged(0, now, now).unwrap());
    let mut bytes = [0; 16_384];
    cache.next_packet(now, &mut bytes).unwrap().unwrap()
}

#[test]
fn same_channel_offers_keep_distinct_expiry_semantics_after_cloning() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    let bootstrap = drain(&mut cache, 0);
    let observation = unchanged(&mut cache, 1);
    assert_eq!(bootstrap[0].channel(), observation.channel());
    assert_eq!(bootstrap[0].expiry_error(), SendError::OriginalExpired);
    assert_eq!(
        observation.clone().expiry_error(),
        SendError::ObservationExpired
    );

    picture(&mut cache, 1, 2);
    let original = drain(&mut cache, 2).pop().unwrap();
    let repair = repair(&mut cache, 2);
    assert_eq!(original.channel(), repair.channel());
    assert_eq!(original.expiry_error(), SendError::OriginalExpired);
    assert_eq!(repair.clone().expiry_error(), SendError::RepairExpired);
    assert_ne!(original, repair);
}

#[test]
fn expired_prepared_repair_does_not_break_the_next_reference() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    drain(&mut cache, 0);
    picture(&mut cache, 1, 1);
    drain(&mut cache, 1);
    let offer = repair(&mut cache, 1);
    let until = offer.send_by_micros();
    cache.authorize_write(&offer, until - 1).unwrap();
    assert_eq!(
        cache.authorize_write(&offer, until),
        Err(SendError::RepairExpired)
    );
    assert!(!cache.needs_recovery());
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.next_deadline(), None);
    picture(&mut cache, 2, until);
    assert_eq!(drain(&mut cache, until).len(), 2);
    assert_eq!(cache.epoch.recovery, RecoveryGeneration::INITIAL);
    assert!(cache.recoveries.iter().all(Option::is_none));
}

#[test]
fn packetized_observations_expire_without_refilling_or_spending_recovery_credit() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    drain(&mut cache, 0);
    // More expiries than the persistent recovery allowance. An idle screen
    // continues to supply real observations of the original picture.
    let mut now = 1;
    for _ in 0..8 {
        let offer = unchanged(&mut cache, now);
        assert_eq!(offer.send_by_micros(), now + HORIZON);
        now = offer.send_by_micros();
        assert_eq!(
            cache.authorize_write(&offer, now),
            Err(SendError::ObservationExpired)
        );
        assert!(!cache.needs_recovery());
        assert_eq!(cache.cached_bytes(), 0);
        assert_eq!(cache.next_deadline(), None);
        let progress = cache.latest_progress().unwrap();
        assert_eq!(progress.descriptor.capture_micros, 0);
        assert_eq!(progress.observed_micros, now - HORIZON);
        now += 1;
    }
    let fresh = unchanged(&mut cache, now);
    cache.authorize_write(&fresh, now).unwrap();
    assert!(cache.recoveries.iter().all(Option::is_none));
    assert_eq!(cache.epoch.recovery, RecoveryGeneration::INITIAL);
}

#[test]
fn optional_expiry_cannot_hide_an_unsent_expired_reference() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    drain(&mut cache, 0);
    let offer = unchanged(&mut cache, 1);
    picture(&mut cache, 1, 2);
    assert_eq!(
        cache.authorize_write(&offer, 2 + HORIZON),
        Err(SendError::OriginalExpired)
    );
    assert!(cache.needs_recovery());
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(
        cache.authorize_write(&offer, 2 + HORIZON),
        Err(SendError::NeedsRecovery)
    );
}

#[test]
fn final_original_still_requires_reference_recovery_after_packetization() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    let offer = drain(&mut cache, 0).pop().unwrap();
    assert_eq!(
        cache.authorize_write(&offer, HORIZON),
        Err(SendError::OriginalExpired)
    );
    // A fixture sink is not actual admission. The existing owner must fence
    // an original it knows was never admitted, even after cache eviction.
    cache.abandon_expired_offer(&offer, HORIZON).unwrap();
    assert!(cache.needs_recovery());
    assert_eq!(cache.recoveries.iter().flatten().count(), 1);
    assert_eq!(
        cache.recovery_progress().unwrap().pipeline,
        PipelineState::Failed
    );
}

#[test]
fn optional_expiry_does_not_mask_foreign_generation_or_clock_failure() {
    let mut cache = cache();
    picture(&mut cache, 0, 0);
    drain(&mut cache, 0);
    let offer = unchanged(&mut cache, 1);
    let mut foreign = self::cache();
    assert_eq!(
        foreign.authorize_write(&offer, offer.send_by_micros()),
        Err(DeliveryError::StaleGeneration.into())
    );
    cache
        .replace(
            MediaEpoch {
                recovery: RecoveryGeneration::INITIAL.next().unwrap(),
                ..cache.epoch
            },
            MediaBindings::new(11, 12, 13, 14).unwrap(),
            2,
        )
        .unwrap();
    assert_eq!(
        cache.authorize_write(&offer, offer.send_by_micros()),
        Err(DeliveryError::StaleGeneration.into())
    );
    assert_eq!(
        cache.authorize_write(&offer, 0),
        Err(DeliveryError::ClockRegression.into())
    );
}
