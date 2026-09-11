//! Detached native work retains the original receiver identity and byte credit.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::*;
use fr_wire::{Channel, MediaLimits, RecoveryChunk, encode_recovery};
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
fn bound(budget: MediaBudget) -> (ReceivePipeline, DecoderBinding) {
    let mut r = ReceivePipeline::new(config(), budget).unwrap();
    let b = r
        .bind_decoder(
            CodecConfigurationGeneration::INITIAL,
            &ProtocolLimits::ABSOLUTE,
            100,
        )
        .unwrap();
    (r, b)
}
fn picture(r: &mut ReceivePipeline) -> ReceivedPicture {
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: 10,
            bytes: b"data",
        },
        2,
        &config().limits,
        &mut bytes,
    )
    .unwrap();
    r.receive(Channel::Recovery, &bytes[..n], 100).unwrap();
    r.take_decodable(101).unwrap().unwrap()
}
#[test]
fn detached_picture_keeps_absolute_display_and_reference_deadlines() {
    let (mut r, b) = bound(MediaBudget::new(&ProtocolLimits::ABSOLUTE).unwrap());
    let p = picture(&mut r);
    assert!(p.within_display_queue_budget());
    assert_eq!(p.display_deadline_us(), 50_100);
    assert_eq!(p.reference_deadline_us(), 2_000_100);
    r.tick(70_000).unwrap();
    assert!(p.is_live());
    assert_eq!(b.check_picture(&p), Ok(()));
    assert_eq!(
        r.complete_decode(&p, 70_001).unwrap().display_deadline_us(),
        50_100
    );
}
#[test]
fn equal_numeric_receivers_and_shared_budget_cannot_exchange_decode_jobs() {
    let budget = MediaBudget::new(&ProtocolLimits::ABSOLUTE).unwrap();
    let (mut first, a) = bound(budget.clone());
    let (mut second, b) = bound(budget);
    let p = picture(&mut first);
    assert_eq!(b.check_picture(&p), Err(DeliveryError::DecodeMismatch));
    assert_eq!(second.state(), ReceiveState::AwaitingRecovery);
    assert_eq!(a.check_picture(&p), Ok(()));
    first.complete_decode(&p, 102).unwrap();
    assert!(second.tick(102).is_ok());
}
#[test]
fn cancelled_decode_fences_callbacks_but_does_not_free_owned_bytes_early() {
    let budget = MediaBudget::new(&ProtocolLimits::ABSOLUTE).unwrap();
    let (mut r, b) = bound(budget.clone());
    let p = picture(&mut r);
    let usage = budget.usage();
    p.cancel_decode();
    assert!(!p.is_live());
    assert_eq!(b.check_picture(&p), Err(DeliveryError::DecodeMismatch));
    assert!(r.tick(102).is_err());
    assert_eq!(r.state(), ReceiveState::Closed);
    assert_eq!(budget.usage(), usage);
    assert_eq!(p.bytes(), b"data");
    drop(p);
    assert_eq!(budget.usage().bytes, 0);
    assert_eq!(budget.usage().pictures, 0);
}
#[test]
fn old_job_cancellation_never_closes_a_replacement_receiver() {
    let (mut r, mut b) = bound(MediaBudget::new(&ProtocolLimits::ABSOLUTE).unwrap());
    let p = picture(&mut r);
    let mut epoch = config().epoch;
    epoch.recovery = epoch.recovery.next().unwrap();
    r.replace(epoch, MediaBindings::new(5, 6, 7, 8).unwrap(), 102)
        .unwrap();
    let fresh = r
        .bind_decoder(epoch.configuration, &ProtocolLimits::ABSOLUTE, 103)
        .unwrap();
    p.cancel_decode();
    b.revoke();
    assert!(fresh.check(&r).is_ok());
    assert!(r.tick(104).is_ok());
    assert_eq!(r.state(), ReceiveState::AwaitingRecovery);
}
