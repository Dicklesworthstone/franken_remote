//! Deterministic receiver/ownership contracts for draining obsolete native work.
//! Byte payloads and decode acknowledgements are fixtures, not HEVC execution.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    BudgetUsage, DecoderBinding, DeliveryError, MediaBindings, MediaBudget, MediaEpoch,
    ReceiveConfig, ReceivePipeline, ReceivePolicy, ReceiveState,
};
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, RecoveryChunk, encode_fragment,
    encode_recovery,
};

fn recovery(receiver: &mut ReceivePipeline, cfg: ReceiveConfig, frame: u64, now: u64) {
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame,
            total_bytes: 4,
            offset: 0,
            capture_micros: now,
            bytes: b"fake",
        },
        cfg.bindings.for_channel(Channel::Recovery),
        &cfg.limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], now)
        .unwrap();
}
fn predicted(receiver: &mut ReceivePipeline, cfg: ReceiveConfig, frame: u64, now: u64) {
    let mut bytes = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame,
                reference: Some(frame - 1),
                capture_micros: frame,
                total_bytes: 4,
                stride: 4,
            },
            index: 0,
            bytes: b"next",
        },
        cfg.bindings.for_channel(Channel::Video),
        &cfg.limits,
        &mut bytes,
    )
    .unwrap();
    receiver.receive(Channel::Video, &bytes[..n], now).unwrap();
}
fn reordered() -> (ReceivePipeline, DecoderBinding, ReceiveConfig) {
    let cfg = ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy {
            reference_budget_micros: 200_000,
            ..ReceivePolicy::default()
        },
    };
    let mut receiver =
        ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
    let decoder = receiver
        .bind_decoder(cfg.epoch.configuration, cfg.limits.protocol(), 0)
        .unwrap();
    recovery(&mut receiver, cfg, 0, 0);
    let initial = receiver.take_decodable(0).unwrap().unwrap();
    receiver.complete_decode(&initial, 0).unwrap();
    drop(initial);
    predicted(&mut receiver, cfg, 2, 10_000);
    predicted(&mut receiver, cfg, 1, 150_000);
    (receiver, decoder, cfg)
}

#[test]
fn reordered_tail_expiry_fences_view_but_retains_native_borrow_until_retirement() {
    let (mut receiver, decoder, _) = reordered();
    let borrowed = receiver.take_decodable(150_000).unwrap().unwrap();
    assert_eq!(borrowed.descriptor().frame, 1);
    assert_eq!(borrowed.reference_deadline_us(), 350_000);
    assert_eq!(receiver.reference_deadline(), Some(210_000));
    assert_eq!(receiver.budget_usage().pictures, 2);
    assert_eq!(receiver.tick(210_000), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert!(!borrowed.is_live());
    assert_eq!(receiver.budget_usage().pictures, 1);
    assert!(receiver.budget_usage().bytes >= 4);
    decoder.check_recovery(&receiver).unwrap();
    assert!(decoder.check(&receiver).is_err());
    assert!(matches!(
        receiver.complete_decode(&borrowed, 210_001),
        Err(DeliveryError::StaleGeneration)
    ));
    drop(borrowed);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    decoder.check_recovery(&receiver).unwrap();
}

#[test]
fn retiring_old_borrow_cannot_revoke_or_free_the_replacement_decoders_picture() {
    let (mut receiver, original, mut cfg) = reordered();
    let borrowed = receiver.take_decodable(150_000).unwrap().unwrap();
    assert_eq!(receiver.tick(210_000), Err(DeliveryError::ReferenceExpired));
    original.check_recovery(&receiver).unwrap();
    cfg.epoch.recovery = cfg.epoch.recovery.next().unwrap();
    cfg.bindings = MediaBindings::new(11, 12, 13, 14).unwrap();
    receiver.replace(cfg.epoch, cfg.bindings, 210_001).unwrap();
    let fresh = receiver
        .bind_decoder(cfg.epoch.configuration, cfg.limits.protocol(), 210_002)
        .unwrap();
    recovery(&mut receiver, cfg, 3, 210_003);
    assert_eq!(receiver.budget_usage().pictures, 2);
    borrowed.cancel_decode();
    drop(borrowed);
    assert_eq!(receiver.budget_usage().pictures, 1);
    fresh.check(&receiver).unwrap();
    assert!(original.check_recovery(&receiver).is_err());
    let picture = receiver.take_decodable(210_004).unwrap().unwrap();
    assert_eq!(picture.descriptor().frame, 3);
    receiver.complete_decode(&picture, 210_005).unwrap();
    drop(picture);
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    fresh.check(&receiver).unwrap();
}

#[test]
fn expiry_at_dequeue_has_no_native_borrow_and_preserves_original_recovery_owner() {
    let (mut receiver, decoder, _) = reordered();
    assert_eq!(receiver.budget_usage().pictures, 2);
    receiver.tick(209_999).unwrap();
    assert!(matches!(
        receiver.take_decodable(210_000),
        Err(DeliveryError::ReferenceExpired)
    ));
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    decoder.check_recovery(&receiver).unwrap();
    assert!(decoder.check(&receiver).is_err());
}
