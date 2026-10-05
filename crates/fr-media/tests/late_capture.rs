//! Deterministic sender-state evidence, not native HEVC or live-network proof.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryError, DeliveryMode, MediaBindings, MediaEpoch, PacketOffer, SendCache, SendError,
    SendPolicy,
};
use fr_wire::{FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};

fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap()
}
fn epoch() -> MediaEpoch {
    MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn cache(policy: SendPolicy) -> SendCache {
    SendCache::new(
        limits(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch(),
        policy,
    )
    .unwrap()
}
fn progress(frame: u64, reference: Option<u64>, capture_micros: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            total_bytes: 128,
            stride: limits().fragment_stride(),
            capture_micros,
            reference,
        },
        observed_micros: capture_micros,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn bootstrap(cache: &mut SendCache) -> PacketOffer {
    cache
        .push(
            progress(0, None, 0),
            vec![7; 128],
            DeliveryMode::Recovery,
            0,
        )
        .unwrap();
    let mut bytes = [0; 1150];
    let offer = cache.next_packet(0, &mut bytes).unwrap().unwrap();
    while cache.next_packet(0, &mut bytes).unwrap().is_some() {}
    assert!(!cache.originals_pending());
    offer
}

#[test]
fn late_capture_fences_the_chain_and_prepared_offers_until_recovery() {
    let mut cache = cache(SendPolicy::default());
    let old_offer = bootstrap(&mut cache);
    let now = 250_001;
    assert!(old_offer.send_by_micros() > now);
    assert_eq!(
        cache.push(
            progress(1, Some(0), 1),
            vec![8; 128],
            DeliveryMode::Datagrams,
            now,
        ),
        Err(SendError::OriginalExpired)
    );
    assert!(cache.needs_recovery());
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.cached_pictures(), 0);
    assert!(!cache.can_push_capacity(128));
    assert_eq!(
        cache.authorize_write(&old_offer, now),
        Err(SendError::NeedsRecovery)
    );
    assert_eq!(
        cache.push(
            progress(2, Some(1), now),
            vec![9; 128],
            DeliveryMode::Datagrams,
            now,
        ),
        Err(SendError::NeedsRecovery)
    );

    let next = MediaEpoch {
        recovery: epoch().recovery.next().unwrap(),
        ..epoch()
    };
    cache
        .replace(next, MediaBindings::new(11, 12, 13, 14).unwrap(), now)
        .unwrap();
    assert!(!cache.needs_recovery());
    assert_eq!(
        cache.authorize_write(&old_offer, now),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    cache
        .push(
            progress(2, None, now),
            vec![9; 128],
            DeliveryMode::Recovery,
            now,
        )
        .unwrap();
    let mut bytes = [0; 1150];
    assert!(cache.next_packet(now, &mut bytes).unwrap().is_some());
}

#[test]
fn a_picture_just_before_its_capture_anchored_deadline_is_still_admitted() {
    let mut cache = cache(SendPolicy::default());
    bootstrap(&mut cache);
    cache
        .push(
            progress(1, Some(0), 1),
            vec![8; 128],
            DeliveryMode::Datagrams,
            250_000,
        )
        .unwrap();
    assert!(!cache.needs_recovery());
    assert_eq!(cache.cached_pictures(), 2);
}

#[test]
fn late_recovery_pictures_cannot_bypass_the_subscription_failure_limit() {
    let mut cache = cache(SendPolicy {
        recovery_horizon_micros: 1,
        max_recoveries_per_window: 2,
        recovery_window_micros: 1_000_000,
        ..SendPolicy::default()
    });
    let mut current = epoch();
    for attempt in 0..3_u32 {
        let capture = u64::from(attempt) * 10;
        let now = capture + 1;
        assert_eq!(
            cache.push(
                progress(0, None, capture),
                vec![7; 128],
                DeliveryMode::Recovery,
                now,
            ),
            Err(SendError::OriginalExpired)
        );
        assert!(cache.needs_recovery());
        current.recovery = current.recovery.next().unwrap();
        let base = 11 + 10 * attempt;
        let result = cache.replace(
            current,
            MediaBindings::new(base, base + 1, base + 2, base + 3).unwrap(),
            now,
        );
        if attempt < 2 {
            result.unwrap();
        } else {
            assert_eq!(result, Err(SendError::RecoveryLimitExceeded));
        }
    }
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.next_deadline(), None);
    assert_eq!(cache.tick(2_000_000), Err(SendError::Closed));
}
