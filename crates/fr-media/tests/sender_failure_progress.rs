//! Deterministic sender-state regressions with synthetic encoded payloads.
//! These exercise the production packetizer, not a codec or network qualifier.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryError, DeliveryMode, MediaBindings, MediaEpoch, SendCache, SendError, SendPolicy,
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation,
};

fn cache() -> SendCache {
    SendCache::new(
        MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy::default(),
    )
    .unwrap()
}

fn progress(frame: u64, reference: Option<u64>, capture_micros: u64) -> Progress {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap();
    Progress {
        descriptor: FrameDescriptor {
            frame,
            reference,
            total_bytes: 64,
            stride: limits.fragment_stride(),
            capture_micros,
        },
        observed_micros: capture_micros,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}

fn bootstrapped() -> SendCache {
    let mut cache = cache();
    cache
        .push(progress(0, None, 0), vec![7; 64], DeliveryMode::Recovery, 0)
        .unwrap();
    let mut bytes = [0; 1150];
    // Packetizer emission only. These assertions are not delivery receipts.
    for channel in [Channel::MediaConfig, Channel::Recovery] {
        let offer = cache.next_packet(0, &mut bytes).unwrap().unwrap();
        assert_eq!(offer.channel(), channel);
        cache.authorize_write(&offer, 0).unwrap();
    }
    assert!(cache.next_packet(0, &mut bytes).unwrap().is_none());
    assert!(cache.recovery_progress().is_none());
    cache
}

fn failed(original: Progress) -> Progress {
    Progress {
        pipeline: PipelineState::Failed,
        ..original
    }
}

#[test]
fn earlier_dispatch_failure_keeps_a_notification_but_never_fresh_source_evidence() {
    let mut cache = bootstrapped();
    let source = progress(1, Some(0), 100);
    cache
        .push(source, vec![9; 64], DeliveryMode::Datagrams, 100)
        .unwrap();
    assert_eq!(cache.latest_progress(), Some(source));
    assert_eq!(cache.tick(250_100), Err(SendError::OriginalExpired));
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.cached_pictures(), 0);
    assert!(cache.latest_progress().is_none());
    assert_eq!(cache.recovery_progress(), Some(failed(source)));
    cache.await_recovery_request(250_100).unwrap();
    let until = cache.next_deadline().unwrap();
    // A later streaming turn has no healthy pre-turn snapshot. It must still
    // be able to notify the peer using the cache's original failed descriptor.
    for now in [250_101, 260_000, 300_000] {
        assert_eq!(cache.tick(now), Err(SendError::NeedsRecovery));
        cache.await_recovery_request(now).unwrap();
        assert!(cache.latest_progress().is_none());
        assert_eq!(cache.recovery_progress(), Some(failed(source)));
        assert_eq!(cache.next_deadline(), Some(until));
    }
}

#[test]
fn final_offer_expiry_after_payload_eviction_retains_its_original_descriptor() {
    let mut cache = bootstrapped();
    let source = progress(1, Some(0), 100);
    cache
        .push(source, vec![9; 64], DeliveryMode::Datagrams, 100)
        .unwrap();
    let mut bytes = [0; 1150];
    let announcement = cache.next_packet(100, &mut bytes).unwrap().unwrap();
    assert_eq!(announcement.channel(), Channel::MediaConfig);
    let pending = cache.next_packet(100, &mut bytes).unwrap().unwrap();
    assert_eq!(pending.channel(), Channel::Video);
    // The final offer is prepared but was never admitted. Ordinary cache
    // eviction alone cannot know that; the egress owner supplies the proof.
    cache.tick(pending.send_by_micros()).unwrap();
    assert!(!cache.needs_recovery());
    cache
        .abandon_expired_offer(&pending, pending.send_by_micros())
        .unwrap();
    assert!(cache.latest_progress().is_none());
    assert_eq!(cache.recovery_progress(), Some(failed(source)));
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.cached_pictures(), 0);
}

#[test]
fn late_native_output_not_admitted_to_the_cache_cannot_replace_notification_evidence() {
    let mut cache = bootstrapped();
    let original = cache.latest_progress().unwrap();
    let rejected = progress(1, Some(0), 100);
    assert_eq!(
        cache.push(rejected, vec![9; 64], DeliveryMode::Datagrams, 250_100),
        Err(SendError::OriginalExpired)
    );
    assert!(cache.latest_progress().is_none());
    assert_eq!(cache.recovery_progress(), Some(failed(original)));
    assert_ne!(cache.recovery_progress(), Some(failed(rejected)));
}

#[test]
fn replacement_and_terminal_close_retire_failed_metadata() {
    let mut cache = bootstrapped();
    cache
        .push(progress(1, Some(0), 100), vec![9; 64], DeliveryMode::Datagrams, 100)
        .unwrap();
    assert_eq!(cache.tick(250_100), Err(SendError::OriginalExpired));
    cache.await_recovery_request(250_100).unwrap();
    assert!(cache.recovery_progress().is_some());
    cache
        .replace(
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL.next().unwrap(),
            },
            MediaBindings::new(11, 12, 13, 14).unwrap(),
            250_101,
        )
        .unwrap();
    assert!(cache.recovery_progress().is_none());
    assert!(cache.latest_progress().is_none());
    let next = progress(2, None, 250_101);
    cache
        .push(next, vec![8; 64], DeliveryMode::Recovery, 250_101)
        .unwrap();
    assert_eq!(cache.latest_progress(), Some(next));
    cache.close();
    assert!(cache.latest_progress().is_none());
    assert!(cache.recovery_progress().is_none());
}

#[test]
fn an_expired_first_picture_never_manufactures_a_source_descriptor() {
    let mut cache = cache();
    assert_eq!(
        cache.push(progress(0, None, 0), vec![7; 64], DeliveryMode::Recovery, 2_000_000),
        Err(SendError::OriginalExpired)
    );
    assert!(cache.needs_recovery());
    cache.await_recovery_request(2_000_000).unwrap();
    assert!(cache.recovery_progress().is_none());
    assert!(cache.latest_progress().is_none());
}

#[test]
fn silent_peer_expiry_releases_metadata_without_renewing_the_failure_window() {
    let mut cache = bootstrapped();
    cache
        .push(progress(1, Some(0), 100), vec![9; 64], DeliveryMode::Datagrams, 100)
        .unwrap();
    assert_eq!(cache.tick(250_100), Err(SendError::OriginalExpired));
    cache.await_recovery_request(250_100).unwrap();
    let until = cache.next_deadline().unwrap();
    assert!(cache.recovery_progress().is_some());
    assert_eq!(
        cache.tick(until),
        Err(SendError::Delivery(DeliveryError::RecoveryExpired))
    );
    assert!(cache.recovery_progress().is_none());
    assert!(cache.latest_progress().is_none());
    assert_eq!(cache.tick(until + 1), Err(SendError::Closed));
}
