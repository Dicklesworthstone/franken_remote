//! Real wire/reassembly and memory owners with synthetic compressed bytes.
//! Draining native work does not extend media validity or recycle its charge.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::*;
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, RecoveryChunk, encode_fragment,
    encode_recovery,
};

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn idr(receiver: &mut ReceivePipeline, config: ReceiveConfig, frame: u64, now: u64) {
    let mut packet = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame,
            total_bytes: 4,
            offset: 0,
            capture_micros: 0,
            bytes: b"idr!",
        },
        config.bindings.for_channel(Channel::Recovery),
        &config.limits,
        &mut packet,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &packet[..n], now)
        .unwrap();
}
fn decoding() -> (ReceiveConfig, ReceivePipeline, ReceivedPicture) {
    let c = config();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    idr(&mut receiver, c, 0, 0);
    let initial = receiver.take_decodable(0).unwrap().unwrap();
    receiver.complete_decode(&initial, 1).unwrap();
    drop(initial);
    let mut packet = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                capture_micros: 0,
                total_bytes: 4,
                stride: 4,
            },
            index: 0,
            bytes: b"late",
        },
        c.bindings.for_channel(Channel::Video),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    receiver.receive(Channel::Video, &packet[..n], 10).unwrap();
    let picture = receiver.take_decodable(11).unwrap().unwrap();
    (c, receiver, picture)
}

#[test]
fn completing_at_the_original_reference_boundary_never_produces_a_receipt() {
    for delay in [0, 1, 1_000_000] {
        let (_, mut receiver, picture) = decoding();
        let charged = receiver.budget_usage();
        let until = picture.reference_deadline_us();
        assert_eq!(charged.pictures, 1);
        assert!(matches!(
            receiver.complete_decode(&picture, until + delay),
            Err(DeliveryError::ReferenceExpired)
        ));
        assert!(!picture.is_live());
        assert_eq!(picture.reference_deadline_us(), until);
        assert_eq!(picture.bytes(), b"late");
        assert_eq!(receiver.budget_usage(), charged);
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        drop(picture);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    }
}

#[test]
fn supervised_expiry_keeps_the_exact_native_borrow_charged_until_retirement() {
    let (_, mut receiver, picture) = decoding();
    let until = picture.reference_deadline_us();
    let charged = receiver.budget_usage();
    for current in [until, until + 1, until + 1_000_000] {
        assert_eq!(receiver.tick(current), Err(DeliveryError::ReferenceExpired));
        assert!(matches!(
            receiver.complete_decode(&picture, current),
            Err(DeliveryError::StaleGeneration)
        ));
        assert_eq!(receiver.budget_usage(), charged);
        assert_eq!(picture.bytes(), b"late");
    }
    drop(picture);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
}

#[test]
fn retiring_an_old_borrow_cannot_clear_or_complete_a_new_recovery_generation() {
    let (c, mut receiver, old) = decoding();
    let until = old.reference_deadline_us();
    assert_eq!(receiver.tick(until), Err(DeliveryError::ReferenceExpired));
    let replacement = ReceiveConfig {
        epoch: MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        bindings: MediaBindings::new(5, 6, 7, 8).unwrap(),
        ..c
    };
    receiver
        .replace(replacement.epoch, replacement.bindings, until + 1)
        .unwrap();
    receiver.decoder_configured(until + 1).unwrap();
    // Reusing a numeric frame ID cannot make the old native reply current.
    idr(&mut receiver, replacement, 1, until + 2);
    let current = receiver.take_decodable(until + 3).unwrap().unwrap();
    assert_eq!(receiver.budget_usage().pictures, 2);
    assert!(matches!(
        receiver.complete_decode(&old, until + 4),
        Err(DeliveryError::StaleGeneration)
    ));
    old.cancel_decode();
    drop(old);
    assert!(current.is_live());
    assert_eq!(receiver.budget_usage().pictures, 1);
    let proof = receiver.complete_decode(&current, until + 5).unwrap();
    assert_eq!(proof.epoch(), replacement.epoch);
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    drop(current);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
}
